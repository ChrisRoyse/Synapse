use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use image::{GrayImage, Luma};
use synapse_perception::{
    HudTemplate, TemplateCounterConfig, extract_template_counter_from_region,
};

fn bench_hud_template_match(c: &mut Criterion) {
    let templates = status_templates();
    let region = synthetic_region(&[2, 2, 2, 2, 2, 2, 2, 0, 0, 0]);
    c.bench_function("hud_template_match_180x16_three_templates", |bench| {
        bench.iter(|| {
            let _ = black_box(extract_template_counter_from_region(
                black_box(&region),
                black_box(&templates),
                black_box(TemplateCounterConfig::default()),
            ));
        });
    });
}

fn status_templates() -> Vec<HudTemplate> {
    vec![
        HudTemplate {
            label: "full".to_owned(),
            value: 2,
            image: full_template(),
        },
        HudTemplate {
            label: "half".to_owned(),
            value: 1,
            image: half_template(),
        },
        HudTemplate {
            label: "empty".to_owned(),
            value: 0,
            image: empty_template(),
        },
    ]
}

fn synthetic_region(values: &[u32; 10]) -> GrayImage {
    let full = full_template();
    let half = half_template();
    let empty = empty_template();
    let mut region = GrayImage::from_pixel(180, 16, Luma([8]));
    for (index, value) in values.iter().enumerate() {
        let slot_x = u32::try_from(index).map_or(0, |item| item.saturating_mul(18));
        let x = slot_x.saturating_add(4);
        let template = match value {
            2 => &full,
            1 => &half,
            _ => &empty,
        };
        blit(&mut region, template, x, 3);
    }
    region
}

fn full_template() -> GrayImage {
    GrayImage::from_fn(9, 9, |x, y| {
        if heart_fill(x, y) {
            Luma([230])
        } else if heart_outline(x, y) {
            Luma([120])
        } else {
            Luma([24])
        }
    })
}

fn half_template() -> GrayImage {
    GrayImage::from_fn(9, 9, |x, y| {
        if heart_fill(x, y) && x <= 4 {
            Luma([230])
        } else if heart_outline(x, y) {
            Luma([120])
        } else {
            Luma([24])
        }
    })
}

fn empty_template() -> GrayImage {
    GrayImage::from_fn(9, 9, |x, y| {
        if heart_outline(x, y) {
            Luma([190])
        } else {
            Luma([24])
        }
    })
}

const fn heart_fill(x: u32, y: u32) -> bool {
    matches!(
        (x, y),
        (2..=3 | 5..=6, 1..=2) | (1..=7, 3..=4) | (2..=6, 5) | (3..=5, 6) | (4, 7)
    )
}

const fn heart_outline(x: u32, y: u32) -> bool {
    matches!(
        (x, y),
        (1..=3 | 5..=7, 0)
            | (0 | 8, 2..=4)
            | (1 | 7, 5)
            | (2 | 6, 6)
            | (3 | 5, 7)
            | (4, 8)
    )
}

fn blit(target: &mut GrayImage, source: &GrayImage, x: u32, y: u32) {
    for source_y in 0..source.height() {
        for source_x in 0..source.width() {
            let target_x = x.saturating_add(source_x);
            let target_y = y.saturating_add(source_y);
            if target_x < target.width() && target_y < target.height() {
                target.put_pixel(target_x, target_y, *source.get_pixel(source_x, source_y));
            }
        }
    }
}

criterion_group!(benches, bench_hud_template_match);
criterion_main!(benches);
