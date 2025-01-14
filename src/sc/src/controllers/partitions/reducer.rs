//!
//! # Partition & Partitions Metadata
//!
//! Partition metadata information on cached in the local Controller.
//!
use std::sync::Arc;

use tracing::{debug, warn, info, instrument};

use fluvio_controlplane_metadata::core::MetadataItem;

use crate::stores::partition::{
    PartitionSpec, PartitionAdminStore, ReplicaStatus, PartitionAdminMd, PartitionResolution,
    ElectionPolicy, ElectionScoring,
};
use crate::stores::spu::{SpuAdminStore, SpuAdminMd, SpuLocalStorePolicy};
use crate::stores::actions::WSAction;

type PartitionWSAction = WSAction<PartitionSpec>;

/// Given This is a generated partition from TopicController, It will try to allocate assign replicas
/// to live SPU.
/// ```text
///     Spec
///           name: Topic0-0
///           replication: 2
///     Status
///           state: Init
///
/// Assuming there are 3 SPU's [0,1,2].  It will try allocate SPU and assign leader.
/// Rules are:
///       SPU id must be unique.  
///       SPU leader must be evently distributed from other Partition.
///
/// So after assignment, then Partition will look liks this
///     Spec
///         name: Topic0-0
///         replication: 2
///     Status
///         status: ready
///         spu:  [0,1]
///         leader: 0
///  
/// The SPU 0 then may be have replica map
///
///     Spec
///        id:  0
///     
///      Status
///         replicas: [Topic0-0]
///
///```
/// If there are another topic1 with same number of partiition and replica then, they will
/// have different leader because Topic0-0 already is using spu 0.
#[derive(Debug)]
pub struct PartitionReducer {
    partition_store: Arc<PartitionAdminStore>,
    spu_store: Arc<SpuAdminStore>,
}

impl Default for PartitionReducer {
    fn default() -> Self {
        Self {
            partition_store: PartitionAdminStore::new_shared(),
            spu_store: SpuAdminStore::new_shared(),
        }
    }
}

impl PartitionReducer {
    pub fn new<A, B>(partition_store: A, spu_store: B) -> Self
    where
        A: Into<Arc<PartitionAdminStore>>,
        B: Into<Arc<SpuAdminStore>>,
    {
        Self {
            partition_store: partition_store.into(),
            spu_store: spu_store.into(),
        }
    }

    #[instrument(skip(self, updates))]
    pub async fn process_partition_update(
        &self,
        updates: Vec<PartitionAdminMd>,
    ) -> Vec<PartitionWSAction> {
        // reconcile delete timestamp in the metadata with delete status
        updates
            .into_iter()
            .filter_map(|partition| {
                if partition.ctx().item().is_being_deleted() && !partition.status.is_being_deleted {
                    debug!("set partition: {} to delete", partition.key());
                    Some(PartitionWSAction::UpdateStatus((
                        partition.key,
                        partition.status.set_to_delete(),
                    )))
                } else {
                    None
                }
            })
            .collect()
    }

    ///
    /// based on spu change, update election
    ///
    #[instrument(skip(self, spus))]
    pub async fn update_election_from_spu_changes(
        &self,
        spus: Vec<SpuAdminMd>,
    ) -> Vec<PartitionWSAction> {
        let mut actions = vec![];

        // group spus in terms of online and offline
        let (online_spus, offline_spus): (Vec<SpuAdminMd>, Vec<SpuAdminMd>) =
            spus.into_iter().partition(|v| v.status.is_online());

        // election due to offline spu
        debug!(offline = offline_spus.len(), "offline spus");
        for offline_spu in offline_spus.into_iter() {
            self.force_election_spu_off(offline_spu, &mut actions).await;
        }

        // election due to online spu
        for online_spu in online_spus.into_iter() {
            self.force_election_spu_on(online_spu, &mut actions).await;
        }
        actions
    }

    /// perform election when spu goes offline
    #[instrument(skip(self, offline_spu, actions))]
    async fn force_election_spu_off(
        &self,
        offline_spu: SpuAdminMd,
        actions: &mut Vec<PartitionWSAction>,
    ) {
        info!(
            spu = %offline_spu.key(),
            "starting election when spu went offline",

        );
        let offline_leader_spu_id = offline_spu.spec.id;

        let spu_status = self.spu_store.online_status().await;

        let policy = SimplePolicy::new();

        // go thru each partitions whose leader matches offline spu.
        for partition_kv_epoch in self.partition_store.read().await.values() {
            let partition_kv = partition_kv_epoch.inner();
            // find partition who's leader is same as offline spu
            if partition_kv.spec.leader == offline_leader_spu_id {
                // find suitable leader
                if let Some(candidate_leader) =
                    partition_kv.status.candidate_leader(&spu_status, &policy)
                {
                    info!(
                        partition = %partition_kv.key(),
                        candidate_leader,
                        "suitable online leader has found",
                    );

                    let mut part_kv_change = partition_kv.clone();
                    part_kv_change.spec.leader = candidate_leader;
                    actions.push(PartitionWSAction::UpdateSpec((
                        part_kv_change.key_owned(),
                        part_kv_change.spec,
                    )));
                // change the
                } else {
                    warn!( partition = %partition_kv.key(),"no suitable leader has found");
                    let mut part_kv_change = partition_kv.clone();
                    part_kv_change.status.resolution = PartitionResolution::LeaderOffline;
                    actions.push(PartitionWSAction::UpdateStatus((
                        part_kv_change.key_owned(),
                        part_kv_change.status,
                    )));
                }
            }
        }
    }

    /// perform election when spu become online
    #[instrument(skip(self, online_spu, actions))]
    async fn force_election_spu_on(
        &self,
        online_spu: SpuAdminMd,
        actions: &mut Vec<PartitionWSAction>,
    ) {
        info!(spu = %online_spu.key(),"start election spu went online" );
        let online_leader_spu_id = online_spu.spec.id;

        let policy = SimplePolicy::new();
        // go thru each partitions which are not online and try to promote given online spu

        for partition_kv_epoch in self.partition_store.read().await.values() {
            let partition_kv = partition_kv_epoch.inner();
            if partition_kv.status.is_offline() {
                // we only care about partition who is follower since, leader will set partition status when it start up
                if partition_kv.spec.leader != online_leader_spu_id {
                    for replica_status in partition_kv.status.replica_iter() {
                        if replica_status.spu == online_leader_spu_id
                            && policy
                                .potential_leader_score(replica_status, &partition_kv.status.leader)
                                .is_suitable()
                        {
                            info!(
                                partition = %partition_kv.key(),
                                online_spu = online_leader_spu_id,
                                "suitable online leader has found",
                            );
                            let mut part_kv_change = partition_kv.clone();
                            part_kv_change.spec.leader = online_leader_spu_id;
                            actions.push(PartitionWSAction::UpdateSpec((
                                part_kv_change.key_owned(),
                                part_kv_change.spec,
                            )));
                        }
                    }
                }
            }
        }
    }
}

struct SimplePolicy {}

impl SimplePolicy {
    fn new() -> Self {
        SimplePolicy {}
    }
}

impl ElectionPolicy for SimplePolicy {
    fn potential_leader_score(
        &self,
        replica_status: &ReplicaStatus,
        leader: &ReplicaStatus,
    ) -> ElectionScoring {
        let lag = leader.leo - replica_status.leo;
        if lag < 4 {
            ElectionScoring::Score(lag as u16)
        } else {
            ElectionScoring::NotSuitable
        }
    }
}

// -----------------------------------
//  Unit Tests
//      >> utils::init_logger();
//      >> RUST_LOG=sc_server=trace cargo test <test-name>
// -----------------------------------

#[cfg(test)]
pub mod test {

    /*
    #[test_async]
    async fn test_process_partition_actions_without_partitions() -> Result<(), ()> {
        // utils::init_logger();

        let partition_reducer = PartitionReducer::default();

        let partition_requests: Vec<PartitionLSChange> = vec![
            // action, (topic,replica), (leader,lrs)
            PartitionLSChange::Add((("topic1", 0), vec![1, 2, 3]).into()),
            PartitionLSChange::Add((("topic1", 1), vec![2, 3, 1]).into()),
        ]
        .into();


        // Run Test
        let _actions = partition_reducer
            .process_requests(PartitionChangeRequest::Partition(partition_requests))
            .await
            .expect("actions");



        // partitions
        let _expected_partitions: Actions<PartitionWSAction> = vec![
            PartitionWSAction::UpdateStatus((("topic1", 0), vec![1, 2, 3]).into()),
            PartitionWSAction::UpdateStatus((("topic1", 1), vec![2, 3, 1]).into()),
        ]
        .into();


        // assert_eq!(actions.partitions,expected_partitions);

        // leader message queue

         TODO: Fix this
        let expected_msgs_for_select_spus: SpuNotifyById<ReplicaMsg> = SpuNotifyById::default();
        let mut leader_msgs = gen_leader_msg_vec(vec![
            //action, name, leader, live_replicas
            (TAction::UPDATE, ("topic1", 0), 1, vec![1, 2, 3]),
            (TAction::UPDATE, ("topic1", 1), 2, vec![2, 3, 1]),
        ]);
        expected_msgs_for_select_spus.push(&2, leader_msgs.pop().unwrap());
        expected_msgs_for_select_spus.push(&1, leader_msgs.pop().unwrap());

        assert_eq!(
            msgs_for_spus,
            expected_msgs_for_select_spus
        );

        Ok(())
    }
    */

    /*
    #[test]
    fn test_process_partition_actions_with_partitions() {
        // utils::init_logger();

        let partitions = create_partitions(vec![
            // topic, idx, epoch, replicas
            (("topic1", 0), 0, vec![0, 1, 2]),
            (("topic1", 1), 0, vec![2, 3, 1]),
            (("topic2", 0), 0, vec![1, 2, 0]),
        ]);
        let partition_actions = create_partition_actions(&vec![
            // action, topic, idx, (epoch lrs), Some(epoch, lrs)
            (TAction::ADD, "topic1", 0, (5, vec![0, 1, 2]), None),
            (
                TAction::MOD,
                "topic1",
                1,
                (1, vec![2, 3, 1]),
                Some((0, vec![2, 3, 1])),
            ),
            (TAction::DEL, "topic2", 0, (0, vec![1, 2, 0]), None),
        ]);
        let mut ctx = PartitionContext::default().with_partition_actions(partition_actions);

        // Run Test
        let res = process_partition_actions(
            &partitions,
            ctx.partition_actions(),
            ctx.actions_for_kvs()
        );

        // Validate Result
        assert_eq!(res.is_ok(), true);

        // partitions
        let expected_partitions = create_partitions(vec![
            // topic, idx, epoch, replicas
            (("topic1", 0), 5, vec![0, 1, 2]),
            (("topic1", 1), 1, vec![2, 3, 1]),
        ]);
        assert_eq!(partitions, expected_partitions);

        // partition kvs actions
        let expected_partition_actions: Actions<PartitionKvsAction> = Actions::default();
        assert_eq!(ctx.takeover_actions_for_kvs(), expected_partition_actions);

        // leader messages
        let expected_msgs_for_select_spus: SpuNotifyById<ReplicaMsg> = SpuNotifyById::default();
        let mut leader_msgs = gen_leader_msg_vec(vec![
            //action, name, epoch, leader, live_replicas
            (TAction::UPDATE, ("topic1", 0), 5, 0, vec![0, 1, 2]),
            (TAction::DEL, ("topic2", 0), 0, 1, vec![1, 2, 0]),
            (TAction::UPDATE, ("topic1", 1), 1, 2, vec![2, 3, 1]),
        ]);

        expected_msgs_for_select_spus.push(&2, leader_msgs.pop().unwrap());
        expected_msgs_for_select_spus.push(&1, leader_msgs.pop().unwrap());
        expected_msgs_for_select_spus.push(&0, leader_msgs.pop().unwrap());
        assert_eq!(
            ctx.takeover_msgs_for_select_spus(),
            expected_msgs_for_select_spus
        );
    }
    */
}
