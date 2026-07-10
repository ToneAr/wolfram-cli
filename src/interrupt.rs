use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

static KERNEL_INTERRUPT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn install_ctrlc_handler() -> Result<()> {
    ctrlc::set_handler(|| {
        KERNEL_INTERRUPT_REQUESTED.store(true, Ordering::SeqCst);
    })
    .context("failed to install Ctrl-C handler")
}

pub(crate) fn clear_kernel_interrupt_request() {
    KERNEL_INTERRUPT_REQUESTED.store(false, Ordering::SeqCst);
}

pub(crate) fn take_kernel_interrupt_request() -> bool {
    KERNEL_INTERRUPT_REQUESTED.swap(false, Ordering::SeqCst)
}
