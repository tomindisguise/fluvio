use url::Url;
use http_types::{Request, Response};
use crate::package_id::WithVersion;
use crate::{Result, FluvioIndex, Package, PackageId, Target, TagName};

pub struct HttpAgent {
    base_url: url::Url,
}

impl Default for HttpAgent {
    fn default() -> Self {
        Self {
            base_url: url::Url::parse(crate::INDEX_LOCATION).unwrap(),
        }
    }
}

impl HttpAgent {
    pub fn with_prefix(prefix: &str) -> Result<Self> {
        Ok(Self {
            base_url: Url::parse(crate::INDEX_HOST).unwrap().join(prefix)?,
        })
    }

    pub fn base_url(&self) -> &str {
        self.base_url.as_str()
    }

    pub fn request_index(&self) -> Result<Request> {
        let url = self.base_url.join("index.json")?;
        Ok(Request::get(url))
    }

    pub async fn index_from_response(&self, mut response: Response) -> Result<FluvioIndex> {
        let index: FluvioIndex = response.body_json().await?;
        Ok(index)
    }

    pub fn request_package<T>(&self, id: &PackageId<T>) -> Result<Request> {
        let url =
            self.base_url
                .join(&format!("packages/{}/{}/meta.json", id.group(), id.name()))?;
        Ok(Request::get(url))
    }

    pub async fn package_from_response(&self, mut response: Response) -> Result<Package> {
        let package: Package = response.body_json().await?;
        Ok(package)
    }

    pub fn request_tag(&self, id: &PackageId<WithVersion>, tag: &TagName) -> Result<Request> {
        let url = self.base_url.join(&format!(
            "packages/{group}/{name}/tags/{tag}",
            group = id.group(),
            name = id.name(),
            tag = tag,
        ))?;

        Ok(Request::get(url))
    }

    pub fn request_release_download<T>(
        &self,
        id: &PackageId<T>,
        version: &semver::Version,
        target: &Target,
    ) -> Result<Request> {
        let url = self.base_url.join(&format!(
            "packages/{group}/{name}/{version}/{target}/{name}",
            group = &id.group(),
            name = &id.name(),
            version = version,
            target = target.as_str(),
        ))?;

        Ok(Request::get(url))
    }

    pub fn request_release_checksum<T>(
        &self,
        id: &PackageId<T>,
        version: &semver::Version,
        target: &Target,
    ) -> Result<Request> {
        let url = self.base_url.join(&format!(
            "packages/{group}/{name}/{version}/{target}/{name}.sha256",
            group = &id.group(),
            name = &id.name(),
            version = version,
            target = target.as_str(),
        ))?;

        Ok(Request::get(url))
    }

    pub async fn tag_version_from_response(
        &self,
        tag: &TagName,
        mut response: Response,
    ) -> Result<semver::Version> {
        let string = response.body_string().await?;
        if string.contains("<title>404 Not Found") {
            return Err(crate::Error::TagDoesNotExist(tag.to_string()));
        }
        let version = semver::Version::parse(&string)?;
        Ok(version)
    }

    pub async fn release_from_response(&self, mut response: Response) -> Result<Vec<u8>> {
        let bytes = response.body_bytes().await?;
        Ok(bytes)
    }

    pub async fn checksum_from_response(&self, mut response: Response) -> Result<String> {
        let string = response.body_string().await?;
        Ok(string)
    }
}
