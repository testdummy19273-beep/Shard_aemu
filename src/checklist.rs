//! The first-run checklist (brief §9): typed step IDs the worker reports, and a reducer the
//! first-run screen renders from.

use crate::sdk::manifest::PackageId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepId {
    /// One Shard per folder (always done once the window is open).
    Lock,
    Folder,
    Disk,
    Hypervisor,
    Download(PackageId),
    Extract,
    Accel,
    Device,
    Boot,
    /// Debloat and tweaks.
    Debloat,
    PlayStore,
}

impl StepId {
    pub const ALL: [StepId; 13] = [
        StepId::Lock,
        StepId::Folder,
        StepId::Disk,
        StepId::Hypervisor,
        StepId::Download(PackageId::PlatformTools),
        StepId::Download(PackageId::Emulator),
        StepId::Download(PackageId::SystemImage),
        StepId::Extract,
        StepId::Accel,
        StepId::Device,
        StepId::Boot,
        StepId::Debloat,
        StepId::PlayStore,
    ];

    pub fn label(self) -> String {
        match self {
            StepId::Lock => "Only one Shard in this folder".into(),
            StepId::Folder => "Check the Shard folder".into(),
            StepId::Disk => "Check free disk space".into(),
            StepId::Hypervisor => "Windows Hypervisor Platform".into(),
            StepId::Download(id) => format!("Download {}", id.label()),
            StepId::Extract => "Extract and check the downloads".into(),
            StepId::Accel => "Hardware acceleration (emulator -accel-check)".into(),
            StepId::Device => "Create the Android device".into(),
            StepId::Boot => "First start of Android".into(),
            StepId::Debloat => "Remove preinstalled apps, apply tweaks".into(),
            StepId::PlayStore => "Open the Play Store for sign-in".into(),
        }
    }

    /// Part of setup (download and install) rather than the device's first start.
    pub fn is_setup(self) -> bool {
        !matches!(self, StepId::Boot | StepId::Debloat | StepId::PlayStore)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Pending,
    Running,
    Done(String),
    /// Done, with something the user should know (a path or disk warning).
    Warning(String),
    Failed(String),
}

impl StepStatus {
    pub fn finished(&self) -> bool {
        matches!(self, StepStatus::Done(_) | StepStatus::Warning(_))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: StepId,
    pub status: StepStatus,
    /// (what, fraction) while the step reports progress.
    pub progress: Option<(String, f32)>,
}

/// What the reducer takes: the worker's typed step reports plus the bits of other events it
/// needs (kept separate from `jobs::Event` so the reducer is easy to test).
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Step(StepId, StepStatus),
    Progress {
        label: String,
        fraction: f32,
    },
    /// A job ended with an error (or was cancelled): whatever was running stops there.
    JobFailed {
        cancelled: bool,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Checklist {
    pub rows: Vec<Row>,
}

pub const CANCELLED: &str = "cancelled; Resume continues where it stopped";

impl Checklist {
    /// `set_up`: the SDK and device already exist, so only the first start is left.
    pub fn new(set_up: bool) -> Self {
        let rows = StepId::ALL
            .iter()
            .map(|&id| Row {
                id,
                status: if id == StepId::Lock || (set_up && id.is_setup()) {
                    StepStatus::Done(String::new())
                } else {
                    StepStatus::Pending
                },
                progress: None,
            })
            .collect();
        Self { rows }
    }

    pub fn row(&self, id: StepId) -> Option<&Row> {
        self.rows.iter().find(|r| r.id == id)
    }

    pub fn apply(&mut self, update: Update) {
        match update {
            Update::Step(id, status) => {
                if let Some(r) = self.rows.iter_mut().find(|r| r.id == id) {
                    if status != StepStatus::Running {
                        r.progress = None;
                    }
                    r.status = status;
                }
            }
            Update::Progress { label, fraction } => {
                if let Some(r) = self
                    .rows
                    .iter_mut()
                    .rev()
                    .find(|r| r.status == StepStatus::Running)
                {
                    r.progress = Some((label, fraction.clamp(0.0, 1.0)));
                }
            }
            Update::JobFailed { cancelled, message } => {
                for r in self
                    .rows
                    .iter_mut()
                    .filter(|r| r.status == StepStatus::Running)
                {
                    r.progress = None;
                    r.status = if cancelled {
                        StepStatus::Failed(CANCELLED.into())
                    } else {
                        StepStatus::Failed(message.clone())
                    };
                }
            }
        }
    }

    pub fn setup_done(&self) -> bool {
        self.rows
            .iter()
            .filter(|r| r.id.is_setup())
            .all(|r| r.status.finished())
    }

    pub fn all_done(&self) -> bool {
        self.rows.iter().all(|r| r.status.finished())
    }

    pub fn running(&self) -> bool {
        self.rows.iter().any(|r| r.status == StepStatus::Running)
    }

    pub fn failed(&self) -> Option<&Row> {
        self.rows
            .iter()
            .find(|r| matches!(r.status, StepStatus::Failed(_)))
    }
}
