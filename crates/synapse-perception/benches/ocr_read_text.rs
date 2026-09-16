use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use synapse_core::Rect;
use synapse_perception::read_text;

fn bench_ocr_read_text(c: &mut Criterion) {
    c.bench_function("ocr_small_region_256x64", |bench| {
        let region = Rect {
            x: 0,
            y: 0,
            w: 256,
            h: 64,
        };
        bench.iter(|| {
            let _ = black_box(read_text(black_box(region)));
        });
    });
    c.bench_function("ocr_full_screen_1080p", |bench| {
        let region = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        bench.iter(|| {
            let _ = black_box(read_text(black_box(region)));
        });
    });
}

criterion_group!(benches, bench_ocr_read_text);
criterion_main!(benches);
