use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Barrier;

use crate::apis::{object_kind, Role, RoleRule};
use crate::apis::{ApplicationAttributes, SessionAttributes};
use crate::ctx::{FlameCluster, FlameClusterContext};

use super::{engine, new_ptr};

#[tokio::test]
async fn role_survives_storage_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let urls = [
        format!("sqlite://{}", directory.path().join("flame.db").display()),
        format!("fs://{}", directory.path().join("files").display()),
    ];

    for url in urls {
        let storage = engine::connect(&url).await.unwrap();
        let role = Role {
            name: "reader".to_string(),
            users: vec!["alice".to_string()],
            rules: HashMap::from([(
                object_kind::APPLICATION.to_string(),
                vec![RoleRule {
                    object_id: "example-app".to_string(),
                    operations: vec!["view".to_string()],
                }],
            )]),
        };
        storage.set_role(&role).unwrap();
        drop(storage);

        let reopened = engine::connect(&url).await.unwrap();
        assert_eq!(reopened.find_roles().unwrap(), vec![role]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn role_cache_serializes_writes_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let urls = [
        format!("sqlite://{}", directory.path().join("roles.db").display()),
        format!("fs://{}", directory.path().join("roles-fs").display()),
    ];

    for url in urls {
        let context = FlameClusterContext {
            cluster: FlameCluster {
                storage: url,
                ..Default::default()
            },
            ..Default::default()
        };
        let storage = new_ptr(&context).await.unwrap();
        let barrier = Arc::new(Barrier::new(16));
        let mut writes = Vec::new();
        for index in 0..16 {
            let storage = storage.clone();
            let barrier = barrier.clone();
            writes.push(tokio::spawn(async move {
                let role = Role {
                    name: "reader".to_string(),
                    users: vec![format!("user-{index}")],
                    rules: HashMap::from([(
                        object_kind::APPLICATION.to_string(),
                        vec![RoleRule {
                            object_id: "example-app".to_string(),
                            operations: vec!["view".to_string()],
                        }],
                    )]),
                };
                barrier.wait().await;
                storage.set_role(&role).unwrap();
            }));
        }
        for write in writes {
            write.await.unwrap();
        }

        let stored = storage.get_role("reader").unwrap().unwrap();
        assert_eq!(storage.list_roles().unwrap(), vec![stored.clone()]);
        drop(storage);

        let reopened = new_ptr(&context).await.unwrap();
        assert!(reopened.get_role("reader").unwrap().is_none());
        reopened.load_data().await.unwrap();
        assert_eq!(reopened.get_role("reader").unwrap(), Some(stored));
        reopened.delete_role("reader").unwrap();
        assert!(reopened.get_role("reader").unwrap().is_none());
        drop(reopened);

        let reopened = new_ptr(&context).await.unwrap();
        reopened.load_data().await.unwrap();
        assert!(reopened.list_roles().unwrap().is_empty());
    }
}

#[tokio::test]
async fn sqlite_memory_roles_support_sync_calls() {
    let context = FlameClusterContext {
        cluster: FlameCluster {
            storage: "sqlite::memory:".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let storage = new_ptr(&context).await.unwrap();
    let role = Role {
        name: "reader".to_string(),
        users: vec!["alice".to_string()],
        rules: HashMap::new(),
    };
    storage.set_role(&role).unwrap();
    assert_eq!(storage.get_role("reader").unwrap(), Some(role));
    storage.delete_role("reader").unwrap();
    assert!(storage.list_roles().unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn session_files_have_private_unix_modes() {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    let directory = tempfile::tempdir().unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();

    let fs_path = directory.path().join("files");
    let fs_url = format!("fs://{}", fs_path.display());
    let files = engine::connect(&fs_url).await.unwrap();
    files
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    files
        .create_session(SessionAttributes {
            id: "session".into(),
            application: "app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let sessions = fs_path.join("sessions");
    let session = sessions.join("session");
    let metadata = session.join("metadata");
    assert_eq!(mode(&sessions), 0o700);
    assert_eq!(mode(&session), 0o700);
    assert_eq!(mode(&metadata), 0o600);

    let sqlite_path = directory.path().join("flame.db");
    let sqlite_url = format!("sqlite://{}", sqlite_path.display());
    let sqlite = engine::connect(&sqlite_url).await.unwrap();
    sqlite
        .register_application("app".into(), ApplicationAttributes::default())
        .await
        .unwrap();
    sqlite
        .create_session(SessionAttributes {
            id: "session".into(),
            application: "app".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(mode(&sqlite_path), 0o600);
    for suffix in ["-wal", "-shm"] {
        let sidecar = directory.path().join(format!("flame.db{suffix}"));
        assert!(
            sidecar.exists(),
            "expected SQLite sidecar {}",
            sidecar.display()
        );
        assert_eq!(mode(&sidecar), 0o600);
    }
}
