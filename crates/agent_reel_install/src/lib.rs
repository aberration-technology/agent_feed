use serde::Serialize;
use std::env;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize)]
pub struct DoctorReport {
    pub codex: ToolProbe,
    pub claude: ToolProbe,
    pub codex_home: PathProbe,
    pub claude_home: PathProbe,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolProbe {
    pub name: &'static str,
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PathProbe {
    pub path: PathBuf,
    pub exists: bool,
}

#[must_use]
pub fn doctor_report() -> DoctorReport {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let codex_home = PathBuf::from(&home).join(".codex");
    let claude_home = PathBuf::from(&home).join(".claude");
    DoctorReport {
        codex: ToolProbe {
            name: "codex",
            path: find_in_path("codex"),
        },
        claude: ToolProbe {
            name: "claude",
            path: find_in_path("claude"),
        },
        codex_home: PathProbe {
            exists: codex_home.exists(),
            path: codex_home,
        },
        claude_home: PathProbe {
            exists: claude_home.exists(),
            path: claude_home,
        },
    }
}

#[must_use]
pub fn init_plan(auto: bool, codex: bool, claude: bool) -> Vec<String> {
    let mut steps = vec![
        "create ~/.agent_reel".to_string(),
        "write default privacy-first config".to_string(),
        "write uninstall manifest".to_string(),
    ];
    if auto || codex {
        steps.push("inspect codex binaries and hooks without overwriting".to_string());
    }
    if auto || claude {
        steps.push("inspect claude settings scopes and hooks without overwriting".to_string());
    }
    steps
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    path.is_file()
}
