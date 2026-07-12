use std::{
    env,
    fs::OpenOptions,
    io::Write,
    sync::{Mutex, OnceLock},
    time::Duration,
};

const PROFILE_ENV: &str = "WOLFRAM_CLI_PROFILE";
const PROFILE_WARN_AFTER: Duration = Duration::from_millis(20);

static PROFILER: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();

fn profiler() -> Option<&'static Mutex<std::fs::File>> {
    PROFILER
        .get_or_init(|| {
            let path = env::var_os(PROFILE_ENV)?;
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some(Mutex::new(file))
        })
        .as_ref()
}

pub(crate) fn profile_event(event: impl AsRef<str>) {
    let Some(file) = profiler() else {
        return;
    };
    let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = writeln!(file, "{}", event.as_ref());
}

pub(crate) fn profile_event_with(event: impl FnOnce() -> String) {
    let Some(file) = profiler() else {
        return;
    };
    let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = writeln!(file, "{}", event());
}

pub(crate) fn profile_duration(label: &str, elapsed: Duration, detail: impl AsRef<str>) {
    if elapsed < PROFILE_WARN_AFTER && profiler().is_none() {
        return;
    }
    profile_event(format!(
        "{label}\t{}ms\t{}",
        elapsed.as_millis(),
        detail.as_ref()
    ));
}

pub(crate) fn profile_duration_with(
    label: &str,
    elapsed: Duration,
    detail: impl FnOnce() -> String,
) {
    let Some(file) = profiler() else {
        return;
    };
    let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = writeln!(file, "{label}\t{}ms\t{}", elapsed.as_millis(), detail());
}
