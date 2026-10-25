//! 采集会话状态机。
//!
//! 一次 `getUserMedia` 会先后牵动 TCC 授权、`AVCaptureSession` 启动、通道首帧与
//! 页面侧 `MediaStream` 合成；每一步都可能失败，也可能被用户中途取消（语音按钮再点一次）。
//! 把这些信号收进一个显式状态机，好处是：
//!
//! - **迟到信号不会复活已经停掉的会话**（例如授权窗关得比页面停止还慢，
//!   随后到来的 `Opened` 必须被丢弃，而不是重新开始推帧）；
//! - 副作用（请求授权、开/关采集、发状态事件）全部以 [`Action`] 返回，
//!   调用方（`macos.rs` / bridge 命令）只负责执行，状态机本身零 I/O、可单测；
//! - 页面看到的状态串集中在一处，避免「页面上写 stopping、壳里还在 live」的错位。

use crate::media::permission::StartDecision;

/// 会话阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// 没有会话。
    Idle,
    /// 等系统授权窗。
    Requesting,
    /// 采集正在启动（设备已开，等第一个 sample buffer）。
    Starting,
    /// 正在推帧。
    Live,
    /// 正在停（已下发停止，等回调确认）。
    Stopping,
    /// 上一次启动失败，等页面重试。
    Failed,
}

impl Phase {
    /// @returns 给页面/日志的状态串。
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Requesting => "requesting",
            Phase::Starting => "starting",
            Phase::Live => "live",
            Phase::Stopping => "stopping",
            Phase::Failed => "failed",
        }
    }
}

/// 驱动状态机的信号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    /// 页面请求开始；携带启动决策（授权是否已给、有没有设备）。
    Start(StartDecision),
    /// TCC 授权回调：允许。
    PermissionGranted,
    /// TCC 授权回调：拒绝（带原因串）。
    PermissionRefused(&'static str),
    /// 采集真的跑起来了（收到第一个 sample buffer）。
    Opened,
    /// 页面请求停止（`track.stop()` / 再次点语音按钮）。
    StopRequested,
    /// 采集已完全停止（session `stopRunning` 返回）。
    Stopped,
    /// 采集中途失败（设备被拔、被独占、格式不支持）。
    Failure(&'static str),
}

/// 状态机要求调用方执行的副作用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 弹系统授权窗（`requestAccessForMediaType`）。
    RequestAccess,
    /// 组装并启动 `AVCaptureSession`。
    OpenCapture,
    /// 给页面发通道首帧（JSON 头），随后才开始推二进制帧。
    PublishHeader,
    /// 停止并释放采集资源（必须幂等）。
    CloseCapture,
    /// 发一条 `microphone-state` 事件。
    EmitState {
        /// 状态串（[`Phase::as_str`] 的取值）。
        state: &'static str,
        /// 失败/拒绝原因；正常状态为 `None`。
        reason: Option<&'static str>,
    },
}

/// 采集会话状态机。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    phase: Phase,
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

impl Machine {
    /// @returns 处于 [`Phase::Idle`] 的新状态机。
    pub fn new() -> Self {
        Self { phase: Phase::Idle }
    }

    /// @returns 当前阶段。
    ///
    /// 只有单测读它：生产路径一律按 [`Machine::accepts_frames`] 判断能不能推帧，
    /// 不对外暴露内部阶段（避免页面/桥接层依赖"某一步"而不是"能不能用"）。
    #[cfg(test)]
    pub fn phase(self) -> Phase {
        self.phase
    }

    /// @returns 是否应当把 PCM 帧推给页面（只有 `Live` 才推）。
    pub fn accepts_frames(self) -> bool {
        self.phase == Phase::Live
    }

    /// 推进状态机。
    ///
    /// 未定义/迟到的信号一律被忽略（返回空动作）：宁可少做，也不要让一个已经在
    /// 停止的会话重新开始出帧。
    /// @param signal - 输入信号。
    /// @returns 需要执行的副作用（按顺序）。
    pub fn apply(&mut self, signal: Signal) -> Vec<Action> {
        match (self.phase, signal) {
            // ---- 启动 ----
            (Phase::Idle | Phase::Failed, Signal::Start(decision)) => match decision {
                StartDecision::Start => {
                    self.phase = Phase::Starting;
                    vec![
                        Action::OpenCapture,
                        Action::EmitState {
                            state: Phase::Starting.as_str(),
                            reason: None,
                        },
                    ]
                }
                StartDecision::RequestAccess => {
                    self.phase = Phase::Requesting;
                    vec![
                        Action::RequestAccess,
                        Action::EmitState {
                            state: Phase::Requesting.as_str(),
                            reason: None,
                        },
                    ]
                }
                StartDecision::Refuse(reason) => {
                    self.phase = Phase::Failed;
                    vec![Action::EmitState {
                        state: Phase::Failed.as_str(),
                        reason: Some(reason),
                    }]
                }
            },
            // 已经在启动/运行：重复的 start 什么都不做（页面连点两下不该开两路采集）。
            (
                Phase::Requesting | Phase::Starting | Phase::Live | Phase::Stopping,
                Signal::Start(_),
            ) => Vec::new(),

            // ---- 授权回调 ----
            (Phase::Requesting, Signal::PermissionGranted) => {
                self.phase = Phase::Starting;
                vec![
                    Action::OpenCapture,
                    Action::EmitState {
                        state: Phase::Starting.as_str(),
                        reason: None,
                    },
                ]
            }
            (Phase::Requesting, Signal::PermissionRefused(reason)) => {
                self.phase = Phase::Failed;
                vec![Action::EmitState {
                    state: Phase::Failed.as_str(),
                    reason: Some(reason),
                }]
            }
            // 授权窗回来得太晚，而且会话已经在停：顺手把可能还开着的资源关掉。
            (Phase::Stopping, Signal::PermissionGranted) => {
                self.phase = Phase::Idle;
                vec![
                    Action::CloseCapture,
                    Action::EmitState {
                        state: Phase::Idle.as_str(),
                        reason: None,
                    },
                ]
            }
            (_, Signal::PermissionGranted | Signal::PermissionRefused(_)) => Vec::new(),

            // ---- 采集真的起来了 ----
            (Phase::Starting, Signal::Opened) => {
                self.phase = Phase::Live;
                vec![
                    Action::PublishHeader,
                    Action::EmitState {
                        state: Phase::Live.as_str(),
                        reason: None,
                    },
                ]
            }
            // 迟到的 Opened（会话已停/已失败）必须丢弃，绝不能复活推帧。
            (_, Signal::Opened) => Vec::new(),

            // ---- 停止 ----
            (Phase::Requesting, Signal::StopRequested) => {
                self.phase = Phase::Idle;
                vec![Action::EmitState {
                    state: Phase::Idle.as_str(),
                    reason: None,
                }]
            }
            (Phase::Starting, Signal::StopRequested) => {
                self.phase = Phase::Stopping;
                vec![
                    Action::CloseCapture,
                    Action::EmitState {
                        state: Phase::Stopping.as_str(),
                        reason: None,
                    },
                ]
            }
            (Phase::Live, Signal::StopRequested) => {
                self.phase = Phase::Stopping;
                vec![
                    Action::CloseCapture,
                    Action::EmitState {
                        state: Phase::Stopping.as_str(),
                        reason: None,
                    },
                ]
            }
            (Phase::Stopping, Signal::StopRequested) => Vec::new(),
            (Phase::Idle, Signal::StopRequested) => Vec::new(),
            (Phase::Failed, Signal::StopRequested) => {
                self.phase = Phase::Idle;
                vec![Action::EmitState {
                    state: Phase::Idle.as_str(),
                    reason: None,
                }]
            }
            (Phase::Stopping, Signal::Stopped) => {
                self.phase = Phase::Idle;
                vec![Action::EmitState {
                    state: Phase::Idle.as_str(),
                    reason: None,
                }]
            }
            (_, Signal::Stopped) => Vec::new(),

            // ---- 失败 ----
            (Phase::Starting | Phase::Live, Signal::Failure(reason)) => {
                self.phase = Phase::Failed;
                vec![
                    Action::CloseCapture,
                    Action::EmitState {
                        state: Phase::Failed.as_str(),
                        reason: Some(reason),
                    },
                ]
            }
            // 停止过程中的错误不值得打扰用户：采集本来就要结束了。
            (Phase::Stopping, Signal::Failure(_)) => {
                self.phase = Phase::Idle;
                vec![Action::EmitState {
                    state: Phase::Idle.as_str(),
                    reason: None,
                }]
            }
            (_, Signal::Failure(_)) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states(actions: &[Action]) -> Vec<(&'static str, Option<&'static str>)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::EmitState { state, reason } => Some((*state, *reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_granted_microphone_starts_and_reaches_live() {
        let mut machine = Machine::new();
        assert_eq!(machine.phase(), Phase::Idle);
        let started = machine.apply(Signal::Start(StartDecision::Start));
        assert_eq!(
            started,
            vec![
                Action::OpenCapture,
                Action::EmitState {
                    state: "starting",
                    reason: None
                },
            ]
        );
        assert_eq!(machine.phase(), Phase::Starting);
        assert!(!machine.accepts_frames());
        assert_eq!(machine.phase(), Phase::Starting);

        let opened = machine.apply(Signal::Opened);
        assert_eq!(
            opened,
            vec![
                Action::PublishHeader,
                Action::EmitState {
                    state: "live",
                    reason: None
                },
            ]
        );
        assert_eq!(machine.phase(), Phase::Live);
        assert!(machine.accepts_frames());
    }

    #[test]
    fn an_undetermined_permission_requests_access_first() {
        let mut machine = Machine::new();
        let actions = machine.apply(Signal::Start(StartDecision::RequestAccess));
        assert_eq!(
            actions,
            vec![
                Action::RequestAccess,
                Action::EmitState {
                    state: "requesting",
                    reason: None
                },
            ]
        );
        assert_eq!(machine.phase(), Phase::Requesting);
        // 采集还没开，不该有资源可回收。

        let granted = machine.apply(Signal::PermissionGranted);
        assert_eq!(states(&granted), vec![("starting", None)]);
        assert_eq!(machine.phase(), Phase::Starting);
    }

    #[test]
    fn a_refused_permission_reports_the_reason() {
        let mut machine = Machine::new();
        let actions = machine.apply(Signal::Start(StartDecision::RequestAccess));
        assert_eq!(states(&actions), vec![("requesting", None)]);
        let refused = machine.apply(Signal::PermissionRefused("denied"));
        assert_eq!(
            refused,
            vec![Action::EmitState {
                state: "failed",
                reason: Some("denied")
            }]
        );
        assert_eq!(machine.phase(), Phase::Failed);
        assert!(!machine.accepts_frames());
    }

    #[test]
    fn a_refused_start_never_opens_capture() {
        let mut machine = Machine::new();
        let actions = machine.apply(Signal::Start(StartDecision::Refuse("device-missing")));
        assert_eq!(
            actions,
            vec![Action::EmitState {
                state: "failed",
                reason: Some("device-missing")
            }]
        );
        assert_eq!(machine.phase(), Phase::Failed);
    }

    #[test]
    fn a_second_start_is_ignored() {
        let mut machine = Machine::new();
        machine.apply(Signal::Start(StartDecision::Start));
        assert!(
            machine
                .apply(Signal::Start(StartDecision::Start))
                .is_empty()
        );
        assert_eq!(machine.phase(), Phase::Starting);
        machine.apply(Signal::Opened);
        assert!(
            machine
                .apply(Signal::Start(StartDecision::Start))
                .is_empty()
        );
        assert_eq!(machine.phase(), Phase::Live);
    }

    #[test]
    fn stopping_a_live_session_closes_capture_then_goes_idle() {
        let mut machine = Machine::new();
        machine.apply(Signal::Start(StartDecision::Start));
        machine.apply(Signal::Opened);
        let stopping = machine.apply(Signal::StopRequested);
        assert_eq!(
            stopping,
            vec![
                Action::CloseCapture,
                Action::EmitState {
                    state: "stopping",
                    reason: None
                },
            ]
        );
        assert_eq!(machine.phase(), Phase::Stopping);
        assert!(!machine.accepts_frames());
        // 重复的停止请求不再触发第二次关闭。
        assert!(machine.apply(Signal::StopRequested).is_empty());
        let stopped = machine.apply(Signal::Stopped);
        assert_eq!(
            stopped,
            vec![Action::EmitState {
                state: "idle",
                reason: None
            }]
        );
        assert_eq!(machine.phase(), Phase::Idle);
    }

    #[test]
    fn stopping_while_starting_still_releases_the_capture() {
        let mut machine = Machine::new();
        machine.apply(Signal::Start(StartDecision::Start));
        let stopping = machine.apply(Signal::StopRequested);
        assert_eq!(
            stopping,
            vec![
                Action::CloseCapture,
                Action::EmitState {
                    state: "stopping",
                    reason: None
                },
            ]
        );
        // 之后到来的 Opened（启动竞态）必须被丢弃。
        assert!(machine.apply(Signal::Opened).is_empty());
        assert_eq!(machine.phase(), Phase::Stopping);
        machine.apply(Signal::Stopped);
        assert_eq!(machine.phase(), Phase::Idle);
    }

    #[test]
    fn stopping_while_waiting_for_permission_needs_no_capture_close() {
        let mut machine = Machine::new();
        machine.apply(Signal::Start(StartDecision::RequestAccess));
        let stopping = machine.apply(Signal::StopRequested);
        assert_eq!(
            stopping,
            vec![Action::EmitState {
                state: "idle",
                reason: None
            }]
        );
        assert_eq!(machine.phase(), Phase::Idle);
        // 授权窗后来才回来：仍然不能开采集。
        let late = machine.apply(Signal::PermissionGranted);
        assert!(!late.contains(&Action::OpenCapture));
        assert_eq!(machine.phase(), Phase::Idle);
    }

    #[test]
    fn stopping_an_idle_session_does_nothing() {
        let mut machine = Machine::new();
        assert!(machine.apply(Signal::StopRequested).is_empty());
        assert!(machine.apply(Signal::Stopped).is_empty());
        assert_eq!(machine.phase(), Phase::Idle);
    }

    #[test]
    fn a_failure_while_live_releases_the_capture() {
        let mut machine = Machine::new();
        machine.apply(Signal::Start(StartDecision::Start));
        machine.apply(Signal::Opened);
        let failed = machine.apply(Signal::Failure("busy"));
        assert_eq!(
            failed,
            vec![
                Action::CloseCapture,
                Action::EmitState {
                    state: "failed",
                    reason: Some("busy")
                },
            ]
        );
        assert_eq!(machine.phase(), Phase::Failed);
        assert!(!machine.accepts_frames());
        // 失败后可以重试。
        let retried = machine.apply(Signal::Start(StartDecision::Start));
        assert!(retried.contains(&Action::OpenCapture));
        assert_eq!(machine.phase(), Phase::Starting);
    }

    #[test]
    fn a_failure_while_stopping_is_swallowed() {
        let mut machine = Machine::new();
        machine.apply(Signal::Start(StartDecision::Start));
        machine.apply(Signal::Opened);
        machine.apply(Signal::StopRequested);
        let failed = machine.apply(Signal::Failure("busy"));
        assert_eq!(
            failed,
            vec![Action::EmitState {
                state: "idle",
                reason: None
            }]
        );
        assert_eq!(machine.phase(), Phase::Idle);
    }

    #[test]
    fn stale_signals_are_ignored() {
        let mut machine = Machine::new();
        // 完全没开始时收到的回调不应该产生任何动作。
        assert!(machine.apply(Signal::Opened).is_empty());
        assert!(machine.apply(Signal::PermissionGranted).is_empty());
        assert!(
            machine
                .apply(Signal::PermissionRefused("denied"))
                .is_empty()
        );
        assert!(machine.apply(Signal::Failure("busy")).is_empty());
        assert_eq!(machine.phase(), Phase::Idle);
    }

    #[test]
    fn frames_are_only_accepted_while_live() {
        let mut machine = Machine::new();
        assert!(!machine.accepts_frames());
        machine.apply(Signal::Start(StartDecision::Start));
        assert!(!machine.accepts_frames());
        machine.apply(Signal::Opened);
        assert!(machine.accepts_frames());
        machine.apply(Signal::StopRequested);
        assert!(!machine.accepts_frames());
        machine.apply(Signal::Stopped);
        assert!(!machine.accepts_frames());
    }

    #[test]
    fn every_phase_name_is_stable() {
        assert_eq!(Phase::Idle.as_str(), "idle");
        assert_eq!(Phase::Requesting.as_str(), "requesting");
        assert_eq!(Phase::Starting.as_str(), "starting");
        assert_eq!(Phase::Live.as_str(), "live");
        assert_eq!(Phase::Stopping.as_str(), "stopping");
        assert_eq!(Phase::Failed.as_str(), "failed");
    }
}
