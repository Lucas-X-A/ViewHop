use super::video::{VideoFormat, VideoMode};

/// A two-second observation window. Missing RTCP is unknown, never zero loss.
#[derive(Clone, Copy, Debug, Default)]
pub struct QualitySample {
    pub overloaded: bool,
    /// RFC 3550 fraction lost, in units of 1/256, from a fresh receiver report.
    pub loss: Option<u8>,
    /// Same observation window as the queue/RTCP sample; absent is unknown.
    pub statistics: Option<crate::VideoStatistics>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QualityTarget {
    pub video: VideoFormat,
    pub bitrate_kbps: u32,
}

/// Conservative additive recovery with sustained-loss and processing hysteresis.
/// The application commits a target only after media/protocol acknowledge it.
pub struct AdaptiveQuality {
    preferred: VideoFormat,
    fallback: Option<VideoFormat>,
    current: QualityTarget,
    bad: u8,
    overloaded: u8,
    good: u8,
    cooldown: u8,
    silent: u8,
    format_changes: bool,
}

impl AdaptiveQuality {
    #[must_use]
    pub fn new(preferred: VideoFormat, fallback: Option<VideoFormat>) -> Self {
        Self {
            preferred,
            fallback,
            current: QualityTarget {
                video: preferred,
                bitrate_kbps: preferred.mode.bitrate_range().0,
            },
            bad: 0,
            overloaded: 0,
            good: 0,
            cooldown: 0,
            silent: 0,
            format_changes: true,
        }
    }

    #[must_use]
    pub fn current(&self) -> QualityTarget {
        self.current
    }

    pub fn observe(&mut self, sample: QualitySample) -> Option<QualityTarget> {
        self.silent = if sample.loss.is_none() {
            self.silent.saturating_add(1)
        } else {
            0
        };
        if self.silent >= 5 {
            self.bad = 0;
            self.good = 0;
        }
        let full_hd = self.current.video.mode == VideoMode::FullHd;
        let statistics = sample
            .statistics
            .filter(|stats| stats.input_fps.is_finite() && stats.encoded_fps.is_finite());
        let frame_margin = !full_hd
            || statistics.is_some_and(|stats| {
                stats.input_fps >= 29.0 && stats.encoded_fps >= 29.0 && stats.dropped_frames <= 1
            });
        // A sparse/static capture does not establish encoder pressure or spare
        // capacity. Only compare output FPS when the source is feeding 30 fps.
        let frame_pressure = full_hd
            && statistics.is_some_and(|stats| stats.input_fps >= 29.0 && stats.encoded_fps < 28.0);
        let local_pressure = sample.overloaded || frame_pressure;
        let bad = sample.loss.is_some_and(|loss| loss >= 8);
        let good = !local_pressure && frame_margin && sample.loss.is_some_and(|loss| loss <= 2);
        if sample.loss.is_some() {
            self.bad = if bad { self.bad.saturating_add(1) } else { 0 };
        }
        self.overloaded = if local_pressure {
            self.overloaded.saturating_add(1)
        } else {
            0
        };
        if sample.loss.is_some() || local_pressure || !frame_margin {
            self.good = if good { self.good.saturating_add(1) } else { 0 };
        }
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return None;
        }
        if sample.loss.is_none() && self.overloaded < if full_hd { 2 } else { 3 } {
            return None;
        }
        let mut target = self.current;
        let (min, max) = target.video.mode.bitrate_range();
        if full_hd && self.overloaded >= 2 && target.bitrate_kbps > min {
            // Preserve Full HD before falling back in resolution. Local encoder
            // pressure is actionable even when there is no fresh RTCP report.
            target.bitrate_kbps = target.bitrate_kbps.saturating_sub(2000).max(min);
        } else if (self.bad >= 3 && target.bitrate_kbps == min) || self.overloaded >= 3 {
            if self.format_changes
                && target.video == self.preferred
                && let Some(fallback) = self.fallback
            {
                target = QualityTarget {
                    video: fallback,
                    bitrate_kbps: VideoMode::Hd.bitrate_range().0,
                };
            }
        } else if self.bad >= 2 {
            target.bitrate_kbps = target.bitrate_kbps.saturating_sub(1000).max(min);
        } else if self.good >= 15 && target.video != self.preferred && self.format_changes {
            target = QualityTarget {
                video: self.preferred,
                bitrate_kbps: self.preferred.mode.bitrate_range().0,
            };
        } else if self.good >= 5 && target.video == self.preferred {
            target.bitrate_kbps = (target.bitrate_kbps + 1000).min(max);
        }
        (target != self.current).then_some(target)
    }

    pub fn applied(&mut self, target: QualityTarget) {
        self.current = target;
        self.reset();
    }

    /// Resets hysteresis after a rejected change. A format rejection also
    /// disables further resolution changes for this connection.
    pub fn rejected(&mut self, format_change: bool) {
        // A receiver/encoder rejecting a format must not cause a retry loop.
        if format_change {
            self.format_changes = false;
        }
        self.reset();
    }

    pub fn reset(&mut self) {
        self.bad = 0;
        self.overloaded = 0;
        self.good = 0;
        self.cooldown = 5;
        self.silent = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wfd::video::H264Profile;

    fn controller() -> AdaptiveQuality {
        let full = VideoFormat {
            mode: VideoMode::FullHd,
            profile: H264Profile::High,
            level: 4,
            r2: false,
        };
        AdaptiveQuality::new(
            full,
            Some(VideoFormat {
                mode: VideoMode::Hd,
                ..full
            }),
        )
    }
    fn feed(c: &mut AdaptiveQuality, sample: QualitySample, windows: usize) {
        for _ in 0..windows {
            if let Some(target) = c.observe(sample) {
                c.applied(target);
            }
        }
    }
    #[test]
    fn unknown_feedback_never_invents_bandwidth_and_brief_loss_does_not_switch() {
        let mut c = controller();
        feed(&mut c, QualitySample::default(), 100);
        assert_eq!(c.current.bitrate_kbps, 8000);
        assert!(
            c.observe(QualitySample {
                loss: Some(80),
                ..QualitySample::default()
            })
            .is_none()
        );
        feed(&mut c, QualitySample::default(), 5);
        assert_eq!(
            c.bad, 0,
            "stale reports must not accumulate as sustained loss"
        );
        assert_eq!(c.current.video.mode, VideoMode::FullHd);
    }
    #[test]
    fn climbs_to_sixteen_then_falls_back_and_recovers_with_hysteresis() {
        let mut c = controller();
        let good = QualitySample {
            loss: Some(0),
            overloaded: false,
            statistics: Some(crate::VideoStatistics {
                input_fps: 30.0,
                encoded_fps: 30.0,
                dropped_frames: 0,
            }),
        };
        feed(&mut c, good, 60);
        assert_eq!(c.current.bitrate_kbps, 16000);
        feed(
            &mut c,
            QualitySample {
                loss: Some(32),
                overloaded: false,
                ..QualitySample::default()
            },
            60,
        );
        assert_eq!(c.current.video.mode, VideoMode::Hd);
        assert_eq!(c.current.bitrate_kbps, 4000);
        feed(&mut c, good, 10);
        assert_eq!(c.current.video.mode, VideoMode::Hd);
        feed(&mut c, good, 10);
        assert_eq!(c.current.video.mode, VideoMode::FullHd);
        assert_eq!(c.current.bitrate_kbps, 8000);
    }

    #[test]
    fn full_hd_requires_frame_headroom_as_well_as_fresh_good_feedback() {
        for statistics in [
            None,
            Some(crate::VideoStatistics {
                input_fps: 5.0,
                encoded_fps: 5.0,
                dropped_frames: 0,
            }),
            Some(crate::VideoStatistics {
                input_fps: 30.0,
                encoded_fps: 28.8,
                dropped_frames: 0,
            }),
            Some(crate::VideoStatistics {
                input_fps: 30.0,
                encoded_fps: 30.0,
                dropped_frames: 2,
            }),
            Some(crate::VideoStatistics {
                input_fps: f64::NAN,
                encoded_fps: 30.0,
                dropped_frames: 0,
            }),
        ] {
            let mut c = controller();
            feed(
                &mut c,
                QualitySample {
                    loss: Some(0),
                    statistics,
                    ..QualitySample::default()
                },
                60,
            );
            assert_eq!(c.current.bitrate_kbps, 8000, "{statistics:?}");
            assert_eq!(c.current.video.mode, VideoMode::FullHd, "{statistics:?}");
        }

        let good = QualitySample {
            loss: Some(0),
            statistics: Some(crate::VideoStatistics {
                input_fps: 30.0,
                encoded_fps: 30.0,
                dropped_frames: 0,
            }),
            ..QualitySample::default()
        };
        let mut c = controller();
        feed(&mut c, good, 4);
        feed(&mut c, QualitySample::default(), 1);
        assert!(
            c.observe(good).is_none(),
            "unknown FPS clears headroom history"
        );
        feed(&mut c, QualitySample { loss: None, ..good }, 60);
        assert_eq!(
            c.current.bitrate_kbps, 8000,
            "FPS alone proves no bandwidth"
        );
    }

    #[test]
    fn full_hd_processing_pressure_reduces_bitrate_before_resolution_without_rtcp() {
        let mut c = controller();
        let good = QualitySample {
            loss: Some(0),
            statistics: Some(crate::VideoStatistics {
                input_fps: 30.0,
                encoded_fps: 30.0,
                dropped_frames: 0,
            }),
            ..QualitySample::default()
        };
        feed(&mut c, good, 60);
        assert_eq!(c.current.bitrate_kbps, 16000);
        feed(&mut c, QualitySample::default(), 5);
        let pressure = QualitySample {
            statistics: Some(crate::VideoStatistics {
                input_fps: 30.0,
                encoded_fps: 24.0,
                dropped_frames: 1,
            }),
            ..QualitySample::default()
        };
        assert!(c.observe(pressure).is_none());
        assert!(
            c.observe(good).is_none(),
            "one slow window must not lower bitrate"
        );
        for bitrate in [14000, 12000, 10000, 8000] {
            assert!(c.observe(pressure).is_none());
            let target = c
                .observe(pressure)
                .expect("sustained FPS pressure must lower bitrate");
            assert_eq!(target.video.mode, VideoMode::FullHd);
            assert_eq!(target.bitrate_kbps, bitrate);
            assert_ne!(c.current(), target, "changes require media acknowledgment");
            c.applied(target);
            feed(&mut c, QualitySample::default(), 5);
        }
        feed(&mut c, pressure, 3);
        assert_eq!(c.current.video.mode, VideoMode::Hd);
        assert_eq!(c.current.bitrate_kbps, 4000);
    }

    #[test]
    fn sixty_fps_preserves_the_per_frame_budget_and_adapts_without_unauthorized_fps_switches() {
        let video = VideoFormat {
            mode: VideoMode::Hd60,
            profile: H264Profile::High,
            level: 2,
            r2: false,
        };
        let mut controller = AdaptiveQuality::new(video, None);
        assert_eq!(
            controller.current().bitrate_kbps,
            2 * VideoMode::Hd.bitrate_range().0
        );
        feed(&mut controller, QualitySample::default(), 60);
        assert_eq!(
            controller.current().bitrate_kbps,
            8000,
            "missing feedback must keep the conservative budget"
        );
        feed(
            &mut controller,
            QualitySample {
                loss: Some(0),
                overloaded: false,
                ..QualitySample::default()
            },
            60,
        );
        assert_eq!(controller.current().bitrate_kbps, 12000);
        feed(
            &mut controller,
            QualitySample {
                loss: Some(64),
                overloaded: false,
                ..QualitySample::default()
            },
            60,
        );
        assert_eq!(controller.current().bitrate_kbps, 8000);
        assert_eq!(
            controller.current().video,
            video,
            "the TV did not authorize live FPS changes"
        );
    }

    #[test]
    fn sustained_processing_pressure_falls_back_without_rtcp_and_rejection_stops_retries() {
        let mut c = controller();
        let pressure = QualitySample {
            overloaded: true,
            loss: None,
            ..QualitySample::default()
        };
        let target = (0..3).find_map(|_| c.observe(pressure)).unwrap();
        assert_eq!(target.video.mode, VideoMode::Hd);
        c.rejected(true);
        assert!((0..100).all(|_| c.observe(pressure).is_none()));
        assert_eq!(c.current.video.mode, VideoMode::FullHd);
    }
}
