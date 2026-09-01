//! 可重复的长文档核心基准。
//!
//! 仅在 `--features benchmark` 下构建，不进入正式安装包。

#![allow(dead_code)]

#[path = "../export.rs"]
mod export;
#[path = "../html_image.rs"]
mod html_image;
#[path = "../markdown.rs"]
mod markdown;
#[path = "../storage.rs"]
mod storage;
#[path = "../theme.rs"]
mod theme;
#[cfg(any(target_os = "windows", target_os = "macos"))]
#[path = "../web_preview.rs"]
mod web_preview;

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

const SIZES: &[(&str, usize)] = &[
    ("100-kib", 100 * 1024),
    ("1-mib", 1024 * 1024),
    ("10-mib", 10 * 1024 * 1024),
];

#[derive(Serialize)]
struct BenchmarkReport {
    schema_version: u32,
    generated_at_unix_ms: u128,
    profile: &'static str,
    size_label: String,
    source_bytes: usize,
    iterations: usize,
    blocks: usize,
    events: usize,
    browser_html_bytes: usize,
    export_html_bytes: usize,
    parse: Timing,
    browser_html: Timing,
    export_html: Timing,
    incremental_edit: IncrementalEditMetrics,
    memory: MemoryStages,
}

#[derive(Serialize)]
struct Timing {
    median_ms: f64,
    p95_ms: f64,
    min_ms: f64,
    max_ms: f64,
}

#[derive(Serialize)]
struct IncrementalEditMetrics {
    timing: Timing,
    changed_blocks: usize,
    changed_virtual_chunks: usize,
    full_navigation_count: usize,
}

#[derive(Serialize, Default, Clone, Copy)]
struct MemorySample {
    working_set_mib: f64,
    peak_working_set_mib: f64,
    pagefile_mib: f64,
}

#[derive(Serialize)]
struct MemoryStages {
    process_start: MemorySample,
    source_loaded: MemorySample,
    parsed_retained: MemorySample,
    browser_html_retained: MemorySample,
    export_html_retained: MemorySample,
}

fn main() -> Result<(), String> {
    let args = Args::parse()?;
    std::fs::create_dir_all(&args.corpus_dir).map_err(|error| error.to_string())?;
    let cases = generate_corpus(&args.corpus_dir)?;
    if args.generate_only {
        return Ok(());
    }
    let selected = cases
        .into_iter()
        .find(|(label, _)| label == &args.size)
        .ok_or_else(|| format!("未知规模：{}，可选 100-kib、1-mib、10-mib", args.size))?;
    let report = benchmark_case(&selected.0, &selected.1)?;
    let json = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&args.output, &json).map_err(|error| error.to_string())?;
    println!("{json}");
    Ok(())
}

struct Args {
    size: String,
    corpus_dir: PathBuf,
    output: PathBuf,
    generate_only: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut size = "100-kib".to_string();
        let mut corpus_dir = PathBuf::from("artifacts/performance/corpus");
        let mut output = PathBuf::from("artifacts/performance/core.json");
        let mut generate_only = false;
        let mut args = std::env::args().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--size" => size = args.next().ok_or("--size 缺少参数")?,
                "--corpus-dir" => {
                    corpus_dir = PathBuf::from(args.next().ok_or("--corpus-dir 缺少参数")?)
                }
                "--output" => output = PathBuf::from(args.next().ok_or("--output 缺少参数")?),
                "--generate-only" => generate_only = true,
                "--help" | "-h" => {
                    println!(
                        "markdown-benchmark --size <100-kib|1-mib|10-mib> --corpus-dir <目录> --output <JSON>"
                    );
                    std::process::exit(0);
                }
                other => return Err(format!("未知参数：{other}")),
            }
        }
        Ok(Self {
            size,
            corpus_dir,
            output,
            generate_only,
        })
    }
}

fn generate_corpus(directory: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut cases = Vec::new();
    for &(label, target_bytes) in SIZES {
        let path = directory.join(format!("long-document-{label}.md"));
        let text = generated_document(target_bytes);
        debug_assert_eq!(text.len(), target_bytes);
        std::fs::write(&path, text).map_err(|error| error.to_string())?;
        cases.push((label.to_string(), path));
    }
    Ok(cases)
}

fn generated_document(target_bytes: usize) -> String {
    const SECTION: &str = r#"
## 章节：从问题到结构

这一节包含 **关键结论**、[参考链接](https://example.com) 和 `inline_code`，用于模拟真实中文技术文档。

- 第一项说明状态、约束和失败处理；
- 第二项说明模块边界和接口契约；
- 第三项说明验证条件和恢复路径。

| 字段 | 类型 | 规则 |
| --- | --- | --- |
| document | ParsedDocument | 单次解析生成 |
| status | State | 操作结束后满足不变量 |

> 外部修改与本地修改同时出现时，保留本地文本并进入冲突状态。

```rust
fn validate(input: &str) -> bool {
    !input.is_empty() && input.len() <= 10 * 1024 * 1024
}
```

---
"#;
    let mut output = String::with_capacity(target_bytes);
    output.push_str("# Markdown 长文档性能基准\n\n");
    let mut index = 1usize;
    while output.len() + SECTION.len() + 32 <= target_bytes {
        output.push_str(&format!("<!-- section:{index} -->\n"));
        output.push_str(SECTION);
        index += 1;
    }
    if output.len() < target_bytes {
        output.push_str("\n<!-- padding:");
        while output.len() + 4 < target_bytes {
            output.push('x');
        }
        while output.len() < target_bytes {
            output.push(' ');
        }
    }
    output.truncate(target_bytes);
    output
}

fn benchmark_case(label: &str, path: &Path) -> Result<BenchmarkReport, String> {
    let process_start = process_memory();
    let source = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let source_loaded = process_memory();
    let iterations = match source.len() {
        0..=200_000 => 25,
        200_001..=2_000_000 => 8,
        _ => 3,
    };

    let parse = measure(iterations, || {
        black_box(markdown::parse_document(black_box(&source)))
    });
    let parsed = markdown::parse_document(&source);
    let parsed_retained = process_memory();

    let browser_html = measure(iterations, || {
        black_box(browser_document(black_box(&parsed)))
    });
    let browser_document = browser_document(&parsed);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let browser_html_bytes = browser_document.total_bytes();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let browser_html_bytes = browser_document.len();
    let browser_html_retained = process_memory();

    let export_options = export::ExportOptions {
        title: "长文档性能基准",
        theme_css: include_str!("../../assets/sspai.css"),
        dark_mode: false,
        theme_spec: theme::ThemeSpec::fallback(false),
        base_directory: path.parent(),
        body_font_size: None,
    };
    let export_html = measure(iterations, || {
        black_box(export::render_styled_html(
            black_box(&parsed),
            export_options,
        ))
    });
    let export_document = export::render_styled_html(&parsed, export_options);
    let export_html_retained = process_memory();

    let edited_source = source.replacen("关键结论", "增量关键结论", 1);
    let edited_parsed = markdown::parse_document_incremental(&parsed, &edited_source)
        .unwrap_or_else(|| markdown::parse_document(&edited_source));
    let changed_blocks = parsed
        .blocks()
        .iter()
        .zip(edited_parsed.blocks())
        .filter(|(old, new)| old != new)
        .count();
    let baseline_incremental_preview =
        incremental_preview(&browser_document, &parsed, &edited_parsed, path.parent());
    let changed_virtual_chunks = baseline_incremental_preview
        .as_ref()
        .map(|next| changed_chunk_count(&browser_document, next))
        .unwrap_or(0);
    let full_navigation_count = usize::from(baseline_incremental_preview.is_none());
    let incremental_edit = IncrementalEditMetrics {
        timing: measure(iterations, || {
            let next = markdown::parse_document_incremental(&parsed, &edited_source)
                .unwrap_or_else(|| markdown::parse_document(&edited_source));
            let preview = incremental_preview(&browser_document, &parsed, &next, path.parent());
            black_box((
                next.blocks().len(),
                preview.map(|value| preview_size(&value)),
            ))
        }),
        changed_blocks,
        changed_virtual_chunks,
        full_navigation_count,
    };

    Ok(BenchmarkReport {
        schema_version: 2,
        generated_at_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        profile: "release",
        size_label: label.to_string(),
        source_bytes: source.len(),
        iterations,
        blocks: parsed.blocks().len(),
        events: parsed.events().len(),
        browser_html_bytes,
        export_html_bytes: export_document.len(),
        parse,
        browser_html,
        export_html,
        incremental_edit,
        memory: MemoryStages {
            process_start,
            source_loaded,
            parsed_retained,
            browser_html_retained,
            export_html_retained,
        },
    })
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn incremental_preview(
    previous_preview: &web_preview::PreviewDocument,
    previous_document: &markdown::ParsedDocument,
    document: &markdown::ParsedDocument,
    base_directory: Option<&Path>,
) -> Option<web_preview::PreviewDocument> {
    web_preview::preview_document_virtual_incremental(
        Some(previous_preview),
        Some(previous_document),
        document,
        base_directory,
    )
    .or_else(|| {
        web_preview::preview_document_incremental(
            Some(previous_preview),
            Some(previous_document),
            document,
            base_directory,
        )
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn incremental_preview(
    _previous_preview: &String,
    _previous_document: &markdown::ParsedDocument,
    _document: &markdown::ParsedDocument,
    _base_directory: Option<&Path>,
) -> Option<String> {
    None
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn changed_chunk_count(
    previous: &web_preview::PreviewDocument,
    next: &web_preview::PreviewDocument,
) -> usize {
    previous.changed_virtual_chunk_count(next)
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn changed_chunk_count(_previous: &String, _next: &String) -> usize {
    0
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn preview_size(document: &web_preview::PreviewDocument) -> usize {
    document.total_bytes()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn preview_size(document: &String) -> usize {
    document.len()
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn browser_document(document: &markdown::ParsedDocument) -> web_preview::PreviewDocument {
    web_preview::preview_document(
        document,
        include_str!("../../assets/sspai.css"),
        None,
        None,
        None,
    )
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn browser_document(document: &markdown::ParsedDocument) -> String {
    export::render_html(document)
}

fn measure<T>(iterations: usize, mut operation: impl FnMut() -> T) -> Timing {
    let _ = black_box(operation());
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let value = black_box(operation());
        samples.push(started.elapsed());
        drop(value);
    }
    samples.sort_unstable();
    Timing {
        median_ms: millis(samples[samples.len() / 2]),
        p95_ms: millis(
            samples[((samples.len() as f64 * 0.95).ceil() as usize - 1).min(samples.len() - 1)],
        ),
        min_ms: millis(samples[0]),
        max_ms: millis(samples[samples.len() - 1]),
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(target_os = "windows")]
fn process_memory() -> MemorySample {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: GetCurrentProcess returns a process pseudo-handle valid for this call;
    // counters points to a correctly sized writable structure.
    let success = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if success == 0 {
        return MemorySample::default();
    }
    MemorySample {
        working_set_mib: mib(counters.WorkingSetSize),
        peak_working_set_mib: mib(counters.PeakWorkingSetSize),
        pagefile_mib: mib(counters.PagefileUsage),
    }
}

#[cfg(not(target_os = "windows"))]
fn process_memory() -> MemorySample {
    MemorySample::default()
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}
