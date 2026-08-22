//! Debug options for the HDA backend.
//!
//! Set from the audiod CLI (`--skip-reset`, `--skip-codec-init`,
//! `--dump-state`, `--dump-ring`, `--dump-topology`) via [`configure`].

use std::sync::RwLock;

/// Codec command transport override (`--cmd-engine=`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmdEngineKind {
    /// DMA ring buffers (default).
    Corb,
    /// Legacy immediate-command interface (IC/IR/IRS).
    Pio,
}

#[derive(Clone, Copy, Default)]
pub struct DebugOpts {
    /// Skip the full link reset during `Controller::open`.
    pub skip_reset: bool,
    /// Skip `init_playback` (codec assumed pre-configured).
    pub skip_codec_init: bool,
    /// Dump live codec register state after init.
    pub dump_state: bool,
    /// Hexdump the first ring bytes written.
    pub dump_ring: bool,
    /// Walk and log the codec widget topology (DAC/mixer/pin graph).
    pub dump_topology: bool,
    /// Force a specific codec command engine. `None` = CORB/RIRB with
    /// automatic PIO fallback.
    pub cmd_engine: Option<CmdEngineKind>,
}

static OPTS: RwLock<Option<DebugOpts>> = RwLock::new(None);

/// Set the flags from the audiod CLI before opening any backend.
pub fn configure(opts: DebugOpts) {
    *OPTS.write().unwrap() = Some(opts);
}

/// Current debug options (CLI flags only).
pub fn opts() -> DebugOpts {
    OPTS.read().unwrap().unwrap_or_default()
}