use std::path::Path;

use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::forbidden_agent_metadata_write;

pub fn metadata_write_forbidden_reason(
    command: &[String],
    cwd: &Path,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
) -> Option<String> {
    if let Some(targets) = crate::bash::parse_shell_lc_write_redirection_targets(command) {
        for target in targets {
            if let Some(name) =
                forbidden_agent_metadata_write(Path::new(&target), cwd, file_system_sandbox_policy)
            {
                return Some(metadata_write_reason(name));
            }
        }
    }
    None
}

fn metadata_write_reason(name: &str) -> String {
    format!("command targets protected workspace metadata path `{name}`")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use codex_protocol::permissions::FileSystemSandboxPolicy;
    use codex_protocol::protocol::SandboxPolicy;
    use pretty_assertions::assert_eq;

    use super::metadata_write_forbidden_reason;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "codex-metadata-write-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).expect("create tempdir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn legacy_workspace_write_policy(cwd: &Path) -> FileSystemSandboxPolicy {
        let policy = SandboxPolicy::WorkspaceWrite {
            writable_roots: vec![],
            network_access: false,
            exclude_tmpdir_env_var: true,
            exclude_slash_tmp: true,
        };
        FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&policy, cwd)
    }

    #[test]
    fn metadata_write_detector_allows_normal_git_under_parent_repo() {
        let repo = TestDir::new("normal-git-under-parent-repo");
        std::fs::create_dir(repo.path().join(".git")).expect("create parent .git");
        let cwd = repo.path().join("sub");
        std::fs::create_dir(&cwd).expect("create cwd");
        let policy = legacy_workspace_write_policy(&cwd);

        let reason = metadata_write_forbidden_reason(
            &[
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "git status --short".to_string(),
            ],
            &cwd,
            &policy,
        );

        assert_eq!(reason, None);
    }

    #[test]
    fn metadata_write_detector_leaves_direct_writes_to_sandbox_policy() {
        let cwd = TestDir::new("direct-metadata-writes");
        let policy = legacy_workspace_write_policy(cwd.path());

        let reason = metadata_write_forbidden_reason(
            &[
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "touch .git && mkdir -p .codex".to_string(),
            ],
            cwd.path(),
            &policy,
        );

        assert_eq!(reason, None);
    }

    #[test]
    fn metadata_write_detector_blocks_metadata_redirections() {
        let repo = TestDir::new("metadata-write-redirections");
        std::fs::create_dir(repo.path().join(".git")).expect("create parent .git");
        let cwd = repo.path().join("sub");
        std::fs::create_dir(&cwd).expect("create cwd");
        let policy = legacy_workspace_write_policy(&cwd);

        let reason = metadata_write_forbidden_reason(
            &[
                "/bin/bash".to_string(),
                "-lc".to_string(),
                "printf pwned > .git".to_string(),
            ],
            &cwd,
            &policy,
        );

        assert_eq!(
            reason,
            Some("command targets protected workspace metadata path `.git`".to_string())
        );
    }
}
