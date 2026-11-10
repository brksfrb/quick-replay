//! Capture pacing, platform independent so it is tested on Linux too.

const HNS_PER_SEC: i64 = 10_000_000;

/// Windows.Graphics.Capture minimum update interval for an encode rate, in 100 ns units.
///
/// Half the encode interval, not the full one. With a 144 Hz display and 144 fps, composition
/// timestamps jitter around 1/144 s; frames that land a hair early are dropped by an exact limit,
/// so the encoder only saw ~72 new pictures a second. The video loop still takes at most one
/// (the newest) frame per tick, so this does not raise the encoded rate above `fps`.
pub fn min_update_interval(fps: u32) -> i64 {
    HNS_PER_SEC / i64::from(fps.max(1)) / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_limit_leaves_room_for_vblank_jitter() {
        for fps in [30, 60, 90, 120, 144, 240] {
            let encode = HNS_PER_SEC / i64::from(fps);
            let limit = min_update_interval(fps);
            assert!(limit > 0);
            // A frame that arrives up to a quarter interval early must still be delivered.
            assert!(limit <= encode * 3 / 4, "fps {fps}: limit {limit} vs encode {encode}");
        }
        // 144 Hz composition at 1/144 s with 0.2 ms of early jitter passes.
        assert!(min_update_interval(144) < HNS_PER_SEC / 144 - 2_000);
        assert_eq!(min_update_interval(0), HNS_PER_SEC / 2);
    }
}
