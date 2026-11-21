//! 立体声处理：Mid/Side 宽度调整、声像平衡。

/// M/S 立体声宽度。width=1 保持原样，>1 拓宽，<1 收窄。
#[derive(Debug, Clone, Copy)]
pub struct StereoWidener {
    width: f32,
}

impl StereoWidener {
    pub fn new(width: f32) -> Self {
        Self {
            width: width.clamp(0.0, 2.0),
        }
    }

    /// 就地处理交错 L/R 数据。
    pub fn process(&self, samples: &mut [f32]) {
        let w = self.width;
        for frame in samples.chunks_exact_mut(2) {
            let l = frame[0];
            let r = frame[1];
            let mid = (l + r) * 0.5;
            let side = (l - r) * 0.5 * w;
            frame[0] = mid + side;
            frame[1] = mid - side;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_width_one_identity() {
        let mut s = vec![0.2, 0.5, -0.3, 0.8];
        let original = s.clone();
        StereoWidener::new(1.0).process(&mut s);
        for (a, b) in s.iter().zip(original.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn test_width_widens() {
        // 中间声像信号 L==R：拓宽不应改变（side=0）。
        let mut s = vec![0.4, 0.4];
        StereoWidener::new(1.5).process(&mut s);
        assert!((s[0] - 0.4).abs() < 1e-6 && (s[1] - 0.4).abs() < 1e-6);

        // 有差异的信号：side 分量应被放大。
        let mut s = vec![1.0, 0.0];
        StereoWidener::new(1.5).process(&mut s);
        assert!(s[0] > 1.0 && s[1] < 0.0);
    }
}
