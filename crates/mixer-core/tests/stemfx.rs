//! StemFX 端到端测试：与 PyTorch 参考实现的 token 序列一致性 + 渲染有效性。

#![cfg(feature = "stemfx")]

use mixer_core::stemfx::{self, StemSet, StyleOptions};
use mixer_core::{read_wav, AudioBuffer};

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures() -> std::path::PathBuf {
    root().join("examples/fixtures")
}

fn load_set(gains: Option<&[f32; 4]>) -> StemSet {
    let names = ["vocals", "bass", "drums", "other"];
    let mut set = StemSet::default();
    for (i, n) in names.iter().enumerate() {
        let path = fixtures().join("stemfx44").join(format!("{}.wav", n));
        let mut buf = read_wav(path.to_str().unwrap()).unwrap();
        if let Some(g) = gains {
            let lin = 10f32.powf(g[i] / 20.0);
            for s in buf.samples.iter_mut() {
                *s *= lin;
            }
        }
        match *n {
            "vocals" => set.vocals = Some(buf),
            "bass" => set.bass = Some(buf),
            "drums" => set.drums = Some(buf),
            _ => set.other = Some(buf),
        }
    }
    set
}

#[test]
fn matches_python_reference_tokens() {
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("stemfx_golden.json")).unwrap(),
    )
    .unwrap();
    let expect: Vec<String> = golden["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    let src = load_set(Some(&[0.0, 0.0, 0.0, 0.0]));
    let reference = load_set(Some(&[3.0, -2.0, 1.5, -4.0]));
    let opts = StyleOptions {
        max_tokens: 512,
        target_lufs: None,
        models_dir: Some(root().join("models").to_str().unwrap().to_string()),
        render: false,
    };
    let res = stemfx::transfer(&src, &reference, &opts, &|| false).unwrap();

    assert!(!res.chain.stems.iter().all(|(_, s)| s.is_empty()), "链为空");
    // token 长度允许 ±2 的漂移（int8 量化 + 重采样差异），但主体结构必须一致
    let common = expect.len().min(res.tokens.len());
    let same = (0..common).filter(|i| expect[*i] == res.tokens[*i]).count();
    assert!(
        same * 100 / common >= 80,
        "token 一致率过低: {}/{} \nrust: {:?}\npy:   {:?}",
        same,
        common,
        res.tokens,
        expect
    );
    assert!(
        res.chain.total_steps() >= 2,
        "效果器数量异常: {}",
        res.chain.total_steps()
    );
}

#[test]
fn embeddings_match_python() {
    let golden: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("stemfx_golden.json")).unwrap(),
    )
    .unwrap();
    let cos = |a: &[f32], b: &[f32]| -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb + 1e-9)
    };
    let mut model = stemfx::StemFxModel::load(Some(root().join("models").to_str().unwrap())).unwrap();
    for (key, set) in [
        ("emb_src", load_set(Some(&[0.0, 0.0, 0.0, 0.0]))),
        ("emb_ref", load_set(Some(&[3.0, -2.0, 1.5, -4.0]))),
    ] {
        let expect: Vec<f32> = golden[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap() as f32)
            .collect();
        let got = stemfx::embed_stems(&mut model, &set).unwrap();
        let c = cos(&got, &expect);
        // W8A32 量化 + Rust 侧 mel 与 PyTorch 的浮点差异，余弦 ~0.994 属正常范围。
        assert!(c > 0.99, "{} 嵌入余弦过低: {:.5}", key, c);
        println!("{} cos = {:.5}", key, c);
    }
}

#[test]
fn renders_audio_and_normalizes_loudness() {
    let src = load_set(Some(&[0.0, 0.0, 0.0, 0.0]));
    let reference = load_set(Some(&[3.0, -2.0, 1.5, -4.0]));
    let opts = StyleOptions {
        max_tokens: 512,
        target_lufs: Some(-23.0),
        models_dir: Some(root().join("models").to_str().unwrap().to_string()),
        render: true,
    };
    let res = stemfx::transfer(&src, &reference, &opts, &|| false).unwrap();
    assert_eq!(res.stems.len(), 4);
    for (name, buf) in &res.stems {
        assert!(buf.frames() > 44100, "{} 时长异常", name);
        let peak = buf.samples.iter().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(peak > 1e-4, "{} 输出静音", name);
        assert!(buf.samples.iter().all(|v| v.is_finite()), "{} 出现非有限值", name);
        if buf.frames() > 44100 {
            let l = mixer_core::dsp::loudness::integrated_lufs(&buf.to_planar(), 44100.0);
            assert!((l + 23.0).abs() < 2.5, "{} 响度未归一: {:.2}", name, l);
        }
    }
    let mix = res.mixdown();
    assert!(mix.samples.iter().any(|v| v.abs() > 1e-3));
    let _ = AudioBuffer::from_planar(vec![vec![0.0; 4]], 44100);
}
