use std::{net::SocketAddr, path::PathBuf};

pub struct Config {
    pub root: PathBuf,
    pub bind: SocketAddr,
    pub max_upload_bytes: u64,
}

impl Config {
    pub fn from_values(
        root: Option<PathBuf>,
        bind: Option<&str>,
        limit: Option<&str>,
    ) -> anyhow::Result<Self> {
        let root = root
            .filter(|root| !root.as_os_str().is_empty())
            .ok_or_else(|| anyhow::anyhow!("FILE_SERVER_ROOT must be set and nonempty"))?;
        let bind: SocketAddr = bind.unwrap_or("127.0.0.1:3002").parse()?;
        anyhow::ensure!(
            bind.ip().is_loopback(),
            "FILE_SERVER_BIND must be loopback: identity headers require a trusted local proxy"
        );
        let max_upload_bytes = limit.unwrap_or("536870912").parse()?;
        anyhow::ensure!(
            max_upload_bytes > 0,
            "FILE_SERVER_MAX_UPLOAD_BYTES must be positive"
        );
        Ok(Self {
            root,
            bind,
            max_upload_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_nonloopback_bind_and_invalid_upload_limits() {
        for bind in [
            "0.0.0.0:3002",
            "100.64.0.1:3002",
            "[::]:3002",
            "[::ffff:127.0.0.1]:3002",
            "invalid",
        ] {
            assert!(
                Config::from_values(Some("blobs".into()), Some(bind), None).is_err(),
                "{bind}"
            );
        }
        for limit in ["0", "-1", "not-a-number", "18446744073709551616", ""] {
            assert!(
                Config::from_values(Some("blobs".into()), None, Some(limit)).is_err(),
                "{limit}"
            );
        }
    }

    #[test]
    fn config_requires_explicit_root_and_honors_bind_and_limit() {
        assert!(Config::from_values(None, None, None).is_err());
        assert!(Config::from_values(Some(PathBuf::new()), None, None).is_err());
        let default = Config::from_values(Some("blobs".into()), None, None).unwrap();
        assert_eq!(default.root, PathBuf::from("blobs"));
        assert_eq!(
            default.bind,
            "127.0.0.1:3002".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(default.max_upload_bytes, 536_870_912);
        let configured =
            Config::from_values(Some("other".into()), Some("[::1]:1234"), Some("4096")).unwrap();
        assert_eq!(configured.root, PathBuf::from("other"));
        assert_eq!(configured.bind, "[::1]:1234".parse::<SocketAddr>().unwrap());
        assert_eq!(configured.max_upload_bytes, 4096);
    }
}
