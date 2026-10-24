use crate::{ErrorCode, Result, VolumeError};
use cap_std::fs::File;
use serde::Serialize;
use std::io::Write;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn check(&self) -> Result<()> {
        if self.0.load(Ordering::Relaxed) {
            Err(VolumeError::new(ErrorCode::Cancelled))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Writing,
    Verifying,
    CheckingSources,
    Publishing,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Progress {
    pub phase: Phase,
    pub completed_bytes: u64,
    pub total_bytes: u64,
    pub part_index: Option<u32>,
}

#[derive(Default)]
pub struct TaskContext<'a> {
    pub cancellation: CancellationToken,
    pub progress: Option<&'a mut dyn FnMut(Progress)>,
    /// Library-only IO fault seam; never exposed as a production CLI option.
    pub io_fault: Option<&'a mut dyn FnMut(IoPoint) -> std::io::Result<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoPoint {
    Read,
    PayloadWrite,
    PayloadSync,
    PlanDurable,
    BeforePublish,
    AfterPublish,
    BeforeCommitMarker,
    WritingIntentDurable,
    StageDirectoryCreated,
    OwnerWrite,
    OwnerSync,
    StartWrite,
    StartSync,
    BeforeStartLink,
    AfterStartLink,
    JournalWrite,
    JournalSync,
    RecoveryPlanWrite,
    RecoveryPlanSync,
    BeforeRecoveryPlanLink,
    CommitMarkerWrite,
    CommitMarkerSync,
    AfterCommitMarker,
    BeforeCleanupLink,
    AfterCleanupLink,
}
impl TaskContext<'_> {
    pub(crate) fn checkpoint(&mut self, point: IoPoint) -> Result<()> {
        self.cancellation.check()?;
        if let Some(fault) = &mut self.io_fault {
            fault(point).map_err(crate::error::io_error)?;
        }
        self.cancellation.check()
    }

    pub(crate) fn write_payload(&mut self, file: &mut File, bytes: &[u8]) -> Result<()> {
        self.write_all_cancellable(file, bytes)
    }

    fn write_all_cancellable(&mut self, writer: &mut impl Write, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            self.checkpoint(IoPoint::PayloadWrite)?;
            match writer.write(bytes) {
                Ok(0) => {
                    return Err(crate::error::io_error(std::io::Error::from(
                        std::io::ErrorKind::WriteZero,
                    )));
                }
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(crate::error::io_error(error)),
            }
        }
        self.cancellation.check()
    }

    pub(crate) fn sync_payload(&mut self, file: &File) -> Result<()> {
        self.checkpoint(IoPoint::PayloadSync)?;
        file.sync_all().map_err(crate::error::io_error)
    }
    pub(crate) fn report(
        &mut self,
        phase: Phase,
        completed_bytes: u64,
        total_bytes: u64,
        part_index: Option<u32>,
    ) -> Result<()> {
        self.cancellation.check()?;
        if let Some(callback) = &mut self.progress {
            callback(Progress {
                phase,
                completed_bytes,
                total_bytes,
                part_index,
            });
        }
        self.cancellation.check()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, io};

    struct PartialWriter {
        bytes: Vec<u8>,
        actions: VecDeque<io::Result<usize>>,
        cancel: Option<CancellationToken>,
    }
    impl Write for PartialWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let count = self
                .actions
                .pop_front()
                .unwrap_or(Ok(bytes.len()))?
                .min(bytes.len());
            self.bytes.extend_from_slice(&bytes[..count]);
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn partial_and_interrupted_writes_complete_without_skipping_bytes() {
        let mut writer = PartialWriter {
            bytes: vec![],
            actions: VecDeque::from([
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Ok(1),
                Ok(2),
                Ok(3),
            ]),
            cancel: None,
        };
        TaskContext::default()
            .write_all_cancellable(&mut writer, b"abcdef")
            .unwrap();
        assert_eq!(writer.bytes, b"abcdef");
        assert!(writer.actions.is_empty());
    }

    #[test]
    fn partial_write_then_storage_permission_or_connection_failure_returns_error() {
        for kind in [
            io::ErrorKind::StorageFull,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::BrokenPipe,
        ] {
            let mut writer = PartialWriter {
                bytes: vec![],
                actions: VecDeque::from([Ok(2), Err(io::Error::from(kind))]),
                cancel: None,
            };
            let error = TaskContext::default()
                .write_all_cancellable(&mut writer, b"abcdef")
                .unwrap_err();
            assert_eq!(
                error.code,
                if kind == io::ErrorKind::StorageFull {
                    ErrorCode::InsufficientDiskSpace
                } else {
                    ErrorCode::IoError
                }
            );
            assert_eq!(writer.bytes, b"ab");
        }
    }

    #[test]
    fn zero_write_is_an_error_instead_of_an_infinite_retry() {
        let mut writer = PartialWriter {
            bytes: vec![],
            actions: VecDeque::from([Ok(0)]),
            cancel: None,
        };
        assert_eq!(
            TaskContext::default()
                .write_all_cancellable(&mut writer, b"abc")
                .unwrap_err()
                .code,
            ErrorCode::IoError
        );
        assert!(writer.bytes.is_empty());
    }

    #[test]
    fn cancellation_is_checked_between_partial_writes() {
        let token = CancellationToken::default();
        let mut writer = PartialWriter {
            bytes: vec![],
            actions: VecDeque::from([Ok(2)]),
            cancel: Some(token.clone()),
        };
        let error = TaskContext {
            cancellation: token,
            ..TaskContext::default()
        }
        .write_all_cancellable(&mut writer, b"abcdef")
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(writer.bytes, b"ab");
    }
}
