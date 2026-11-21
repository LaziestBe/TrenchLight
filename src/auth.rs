use md5::{Digest, Md5};

use crate::util::to_hex;

#[derive(Debug, Clone)]
pub struct Session {
    pub username: String,
    pub uuid: String,
    pub access_token: String,
    pub user_type: String,
    pub xuid: String,
    pub client_id: String,
    pub online: bool,
}

/// An offline session.
///
/// Mojang's servers never see a token from this launcher. The client is told it
/// is a `legacy` session with no uuid and a zero token, which is enough for
/// singleplayer and for servers that run with `online-mode=false`.
pub fn offline(username: &str) -> Session {
    Session {
        username: username.to_string(),
        uuid: offline_uuid(username),
        access_token: "0".to_string(),
        user_type: "legacy".to_string(),
        xuid: "0".to_string(),
        client_id: "0".to_string(),
        online: false,
    }
}

/// The same UUID Java's `UUID.nameUUIDFromBytes` produces for
/// `("OfflinePlayer:" + name)`: MD5, forced to version 3 / RFC 4122 variant.
pub fn offline_uuid(username: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(format!("OfflinePlayer:{username}").as_bytes());
    let mut digest: [u8; 16] = hasher.finalize().into();
    digest[6] = (digest[6] & 0x0f) | 0x30;
    digest[8] = (digest[8] & 0x3f) | 0x80;
    let hex = to_hex(&digest);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_java_offline_uuid() {
        // Cross-checked against java.util.UUID.nameUUIDFromBytes on JDK 25.
        assert_eq!(offline_uuid("Notch"), "b50ad385-829d-3141-a216-7e7d7539ba7f");
        assert_eq!(offline_uuid("Steve"), "5627dd98-e6be-3c21-b8a8-e92344183641");
        assert_eq!(
            offline_uuid("Tester_123"),
            "5652296d-466c-3b95-8f10-b665dd4283b8"
        );
    }

    #[test]
    fn uuid_is_version_3() {
        let uuid = offline_uuid("Steve");
        assert_eq!(&uuid[14..15], "3");
    }
}
