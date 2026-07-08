use std::{fmt, path::PathBuf};

pub(crate) struct FrontEndClient {
    install_dir: Option<PathBuf>,
    unavailable: bool,
    active: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum FrontEndStatus {
    Active,
    Lazy,
    Ready,
    Disabled,
    Unavailable,
}

impl fmt::Display for FrontEndStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => f.write_str("active"),
            Self::Lazy => f.write_str("lazy"),
            Self::Ready => f.write_str("ready"),
            Self::Disabled => f.write_str("disabled"),
            Self::Unavailable => f.write_str("unavailable"),
        }
    }
}

impl FrontEndClient {
    pub(crate) fn new() -> Self {
        Self {
            install_dir: None,
            unavailable: false,
            active: false,
        }
    }

    pub(crate) fn status(&self) -> FrontEndStatus {
        if self.active {
            FrontEndStatus::Active
        } else if self.unavailable {
            FrontEndStatus::Unavailable
        } else if self.install_dir.is_some() {
            FrontEndStatus::Ready
        } else {
            FrontEndStatus::Lazy
        }
    }
}

pub(crate) fn frontend_status(
    frontend: Option<&std::sync::Arc<std::sync::Mutex<FrontEndClient>>>,
) -> anyhow::Result<FrontEndStatus> {
    let Some(frontend) = frontend else {
        return Ok(FrontEndStatus::Disabled);
    };

    match frontend.try_lock() {
        Ok(frontend) => Ok(frontend.status()),
        Err(std::sync::TryLockError::WouldBlock) => Ok(FrontEndStatus::Active),
        Err(std::sync::TryLockError::Poisoned(_)) => {
            Err(anyhow::anyhow!("FrontEnd lock was poisoned"))
        }
    }
}
