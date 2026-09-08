//! `asr-cli`: the UI-free driver for `asr-core`.
//!
//! This is deliberately the primary way to exercise the pipeline during
//! Phase 0/1 — the plan's whole point is that speaker-diarization accuracy
//! work should not have to wait on the GPUI editor being finished.

mod der;
mod rttm;
mod serve;

use std::path::PathBuf;

use anyhow::{Context, Result};
use asr_core::diarize::{DiarizeModelPaths, DiarizeParams};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "asr-cli", about = "Batch/eval CLI for the transcription pipeline core")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print basic info about a media file (sample rate, channels, duration).
    Info { media: PathBuf },

    /// Build a waveform peak pyramid and write it as JSON.
    Peaks {
        media: PathBuf,
        #[arg(long, default_value = "peaks.json")]
        out: PathBuf,
        #[arg(long, default_value_t = 256)]
        base_samples_per_peak: usize,
    },

    /// Run VAD + speaker diarization and print segments (JSON or RTTM).
    Diarize {
        media: PathBuf,
        #[arg(long, default_value = "models")]
        models_dir: PathBuf,
        // Matches asr_core::diarize::DiarizeParams::default() — see its
        // doc comment for the DER measurement behind this value.
        #[arg(long, default_value_t = 0.9)]
        cluster_threshold: f32,
        #[arg(long)]
        num_speakers: Option<i32>,
        /// Minimum accepted speech-segment duration, in seconds. Segments
        /// shorter than this are dropped by the segmentation model before
        /// clustering ever sees them — lowering it is the lever for
        /// short-utterance ("aizuchi") missed-detection, per the plan doc's
        /// finding that missed detection (not confusion) dominates DER once
        /// cluster_threshold is tuned.
        #[arg(long, default_value_t = 0.3)]
        min_duration_on: f32,
        #[arg(long, default_value_t = 0.5)]
        min_duration_off: f32,
        #[arg(long, default_value_t = 500)]
        max_gap_ms: u64,
        #[arg(long, default_value_t = 60_000)]
        max_merged_ms: u64,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Compare a hypothesis RTTM against a reference RTTM and report DER
    /// with the false-alarm/missed-detection/confusion breakdown.
    Eval {
        #[arg(long)]
        reference: PathBuf,
        #[arg(long)]
        hypothesis: PathBuf,
    },

    /// 探索モード step 1: run VAD + windowed embedding extraction once and
    /// cache the result. This is the expensive step a live threshold
    /// slider must never re-run — see `asr_core::diarize::fast`.
    FastCache {
        media: PathBuf,
        #[arg(long, default_value = "models")]
        models_dir: PathBuf,
        #[arg(long, default_value = "cache.json")]
        out: PathBuf,
        #[arg(long, default_value_t = 1500)]
        window_ms: u64,
        #[arg(long, default_value_t = 750)]
        hop_ms: u64,
    },

    /// 探索モード step 2: re-cluster a cache built by `fast-cache` at a
    /// given threshold. No model inference — this is what a live slider
    /// would call on every move; timed so we can tell whether it's
    /// actually fast enough to feel live.
    FastRecluster {
        cache: PathBuf,
        #[arg(long, default_value_t = 0.3)]
        threshold: f64,
        #[arg(long, default_value_t = 500)]
        max_gap_ms: u64,
        #[arg(long, default_value_t = 60_000)]
        max_merged_ms: u64,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Convenience for the practicality test: load a cache once and
    /// re-cluster it at every given threshold in one process, printing
    /// per-threshold timing and resulting speaker count. This is what
    /// "moving a slider" looks like with the model-loading cost paid only
    /// once, up front.
    FastSweep {
        cache: PathBuf,
        /// Comma-separated thresholds, e.g. "0.1,0.2,0.3,0.4,0.5".
        #[arg(long)]
        thresholds: String,
    },

    /// Serve a built cache over HTTP so a UI (e.g.
    /// `tools/diarization-viewer.html`) can drive the threshold live. See
    /// `src/serve.rs` for why this is a hand-rolled server rather than a
    /// framework.
    Serve {
        cache: PathBuf,
        #[arg(long, default_value_t = 7878)]
        port: u16,
        #[arg(long, default_value_t = 500)]
        max_gap_ms: u64,
        #[arg(long, default_value_t = 60_000)]
        max_merged_ms: u64,
    },

    /// Transcribe one audio file (typically a single already-diarized
    /// chunk) via ElevenLabs Scribe v2. Reads the API key from an
    /// environment variable, never a CLI arg, so it can't end up in shell
    /// history or a process list.
    Transcribe {
        media: PathBuf,
        #[arg(long, default_value = "ja")]
        language: String,
        #[arg(long, default_value = "ELEVENLABS_API_KEY")]
        api_key_env: String,
        /// Comma-separated registered terms to bias toward (keyterms).
        #[arg(long)]
        terms: Option<String>,
    },

    /// Full pipeline, first real end-to-end run: diarize -> merge ->
    /// pad into chunks -> ElevenLabs per chunk -> trim overlap. One
    /// sequential request per diarized segment (see
    /// asr_core::pipeline::run_transcription_stage for why sequential).
    TranscribePipeline {
        media: PathBuf,
        #[arg(long, default_value = "models")]
        models_dir: PathBuf,
        #[arg(long, default_value_t = 0.9)]
        cluster_threshold: f32,
        #[arg(long, default_value_t = 500)]
        max_gap_ms: u64,
        #[arg(long, default_value_t = 60_000)]
        max_merged_ms: u64,
        /// Real-audio context padding on each side of a chunk, per the
        /// plan's short-chunk mitigation.
        #[arg(long, default_value_t = 300)]
        pad_ms: u64,
        #[arg(long, default_value = "ja")]
        language: String,
        #[arg(long, default_value = "ELEVENLABS_API_KEY")]
        api_key_env: String,
        /// Comma-separated ad-hoc terms (keyterms only, no post-ASR
        /// correction — that needs the reading/alias structure a flat
        /// string list doesn't have). Ignored if --terms-db is given.
        #[arg(long)]
        terms: Option<String>,
        /// SQLite DB (see `terms::store`) to load registered terms +
        /// aliases from. When given, terms are sent as keyterms (weight
        /// order, provider-truncated) AND every transcribed segment is
        /// run through `terms::correct_text` (layer 3 alias substitution,
        /// then layer 2 reading-based fuzzy match).
        #[arg(long)]
        terms_db: Option<PathBuf>,
        #[arg(long, default_value = "default")]
        project_id: String,
        /// 0.85 measured too strict against a real live ElevenLabs
        /// response: "アシタカ総研" misheard as "明日化総研" tokenizes
        /// as 明日/化/総研, whose concatenated reading (アスカソウケン)
        /// only scores 0.75 against the term's reading (アシタカソウケン)
        /// -- the exact motivating case for the sliding window never
        /// fired at 0.85 in production. Lowered to 0.7, matching the
        /// value already used in the unit test that demonstrates the
        /// mechanism. See docs/plan.md.
        #[arg(long, default_value_t = 0.7)]
        correction_threshold: f32,
        #[arg(long)]
        out: Option<PathBuf>,
    },

    /// Manage the terminology registry (see the plan doc's 3-layer
    /// correction design and `terms::store`).
    Terms {
        #[command(subcommand)]
        action: TermsAction,
    },
}

#[derive(Subcommand)]
enum TermsAction {
    /// Register a term (and optionally its known alternate surface
    /// forms — e.g. an English-script rendering; see "クラウドゲート問題"
    /// in the plan doc).
    Add {
        db: PathBuf,
        #[arg(long, default_value = "default")]
        project_id: String,
        surface: String,
        #[arg(long)]
        reading: Option<String>,
        #[arg(long, default_value_t = 1.0)]
        weight: f32,
        /// Repeatable: known alternate surface forms for this term.
        #[arg(long = "alias")]
        aliases: Vec<String>,
    },
    /// List registered terms (and their aliases) for a project.
    List {
        db: PathBuf,
        #[arg(long, default_value = "default")]
        project_id: String,
    },
}

#[derive(Clone, clap::ValueEnum)]
enum OutputFormat {
    Json,
    Rttm,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Info { media } => cmd_info(&media),
        Command::Peaks { media, out, base_samples_per_peak } => {
            cmd_peaks(&media, &out, base_samples_per_peak)
        }
        Command::Diarize {
            media,
            models_dir,
            cluster_threshold,
            num_speakers,
            min_duration_on,
            min_duration_off,
            max_gap_ms,
            max_merged_ms,
            format,
            out,
        } => cmd_diarize(
            &media,
            &models_dir,
            cluster_threshold,
            num_speakers,
            min_duration_on,
            min_duration_off,
            max_gap_ms,
            max_merged_ms,
            format,
            out.as_deref(),
        ),
        Command::Eval { reference, hypothesis } => cmd_eval(&reference, &hypothesis),
        Command::FastCache { media, models_dir, out, window_ms, hop_ms } => {
            cmd_fast_cache(&media, &models_dir, &out, window_ms, hop_ms)
        }
        Command::FastRecluster { cache, threshold, max_gap_ms, max_merged_ms, format, out } => {
            cmd_fast_recluster(&cache, threshold, max_gap_ms, max_merged_ms, format, out.as_deref())
        }
        Command::FastSweep { cache, thresholds } => cmd_fast_sweep(&cache, &thresholds),
        Command::Serve { cache, port, max_gap_ms, max_merged_ms } => {
            cmd_serve(&cache, port, max_gap_ms, max_merged_ms)
        }
        Command::Transcribe { media, language, api_key_env, terms } => {
            cmd_transcribe(&media, &language, &api_key_env, terms.as_deref())
        }
        Command::TranscribePipeline {
            media,
            models_dir,
            cluster_threshold,
            max_gap_ms,
            max_merged_ms,
            pad_ms,
            language,
            api_key_env,
            terms,
            terms_db,
            project_id,
            correction_threshold,
            out,
        } => cmd_transcribe_pipeline(
            &media,
            &models_dir,
            cluster_threshold,
            max_gap_ms,
            max_merged_ms,
            pad_ms,
            &language,
            &api_key_env,
            terms.as_deref(),
            terms_db.as_deref(),
            &project_id,
            correction_threshold,
            out.as_deref(),
        ),
        Command::Terms { action } => cmd_terms(action),
    }
}

fn cmd_info(media: &std::path::Path) -> Result<()> {
    let decoded = asr_core::audio::decode_file(media)
        .with_context(|| format!("decoding {}", media.display()))?;

    println!("file:         {}", media.display());
    println!("sample_rate:  {} Hz", decoded.sample_rate);
    println!("channels:     {}", decoded.channels);
    println!("frames:       {}", decoded.frames());
    if let Some(ms) = decoded.duration_ms {
        println!("duration:     {:.2} s", ms as f64 / 1000.0);
    } else {
        println!("duration:     unknown (container did not report one)");
    }
    Ok(())
}

fn cmd_peaks(media: &std::path::Path, out: &std::path::Path, base_samples_per_peak: usize) -> Result<()> {
    let decoded = asr_core::audio::decode_file(media)
        .with_context(|| format!("decoding {}", media.display()))?;
    let mono = decoded.to_mono();
    let pyramid = asr_core::audio::build_pyramid(&mono, decoded.sample_rate, base_samples_per_peak);

    let json = serde_json::to_string(&pyramid)?;
    std::fs::write(out, json).with_context(|| format!("writing {}", out.display()))?;

    println!(
        "wrote {} levels ({} finest-level buckets) to {}",
        pyramid.levels.len(),
        pyramid.levels[0].peaks.len(),
        out.display()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_diarize(
    media: &std::path::Path,
    models_dir: &std::path::Path,
    cluster_threshold: f32,
    num_speakers: Option<i32>,
    min_duration_on: f32,
    min_duration_off: f32,
    max_gap_ms: u64,
    max_merged_ms: u64,
    format: OutputFormat,
    out: Option<&std::path::Path>,
) -> Result<()> {
    let models = DiarizeModelPaths::under(models_dir);
    let params = DiarizeParams {
        cluster_threshold,
        num_speakers,
        min_duration_on,
        min_duration_off,
        ..Default::default()
    };

    let start = std::time::Instant::now();
    let output = asr_core::pipeline::run_diarization_stage(
        media,
        &models,
        &params,
        max_gap_ms,
        max_merged_ms,
    )
    .context("diarization pipeline failed")?;
    let elapsed = start.elapsed();

    let audio_duration_sec = output
        .merged_segments
        .iter()
        .map(|s| s.end_ms)
        .max()
        .unwrap_or(0) as f64
        / 1000.0;
    let realtime_factor = if elapsed.as_secs_f64() > 0.0 {
        audio_duration_sec / elapsed.as_secs_f64()
    } else {
        0.0
    };
    eprintln!(
        "processed {audio_duration_sec:.1}s of audio in {:.1}s ({realtime_factor:.1}x realtime)",
        elapsed.as_secs_f64()
    );

    let rendered = match format {
        OutputFormat::Json => serde_json::to_string_pretty(&output.merged_segments)?,
        OutputFormat::Rttm => {
            let uri = media.file_stem().and_then(|s| s.to_str()).unwrap_or("media");
            rttm::render(uri, &output.merged_segments)
        }
    };

    match out {
        Some(path) => {
            std::fs::write(path, &rendered)?;
            eprintln!("wrote {} segments to {}", output.merged_segments.len(), path.display());
        }
        None => println!("{rendered}"),
    }

    Ok(())
}

fn cmd_eval(reference: &std::path::Path, hypothesis: &std::path::Path) -> Result<()> {
    let reference_segments = rttm::parse(reference)?;
    let hypothesis_segments = rttm::parse(hypothesis)?;

    let components = der::compute_der(&reference_segments, &hypothesis_segments);

    println!("reference:          {}", reference.display());
    println!("hypothesis:         {}", hypothesis.display());
    println!("total ref speech:   {:.2}s", components.total_reference_sec);
    println!("missed detection:   {:.2}s", components.missed_detection_sec);
    println!("false alarm:        {:.2}s", components.false_alarm_sec);
    println!("speaker confusion:  {:.2}s", components.confusion_sec);
    println!("DER:                {:.4} ({:.1}%)", components.der(), components.der() * 100.0);
    println!(
        "(note: speaker label mapping is greedy, not full Hungarian assignment \
         — see crate docs in der.rs)"
    );

    Ok(())
}

fn cmd_fast_cache(
    media: &std::path::Path,
    models_dir: &std::path::Path,
    out: &std::path::Path,
    window_ms: u64,
    hop_ms: u64,
) -> Result<()> {
    use asr_core::diarize::fast::{build_embedding_cache, FastModelPaths, WindowParams};

    let models = FastModelPaths::under(models_dir);
    let params = WindowParams { window_ms, hop_ms, ..Default::default() };

    let decode_start = std::time::Instant::now();
    let pcm = asr_core::audio::load_for_pipeline(media).context("decoding + resampling")?;
    let decode_elapsed = decode_start.elapsed();

    let cache_start = std::time::Instant::now();
    let cache = build_embedding_cache(&pcm, &models, &params).context("building embedding cache")?;
    let cache_elapsed = cache_start.elapsed();

    let audio_sec = pcm.len() as f64 / asr_core::audio::TARGET_SAMPLE_RATE as f64;
    eprintln!(
        "decode+resample: {:.1}s of audio in {:.2}s",
        audio_sec,
        decode_elapsed.as_secs_f64()
    );
    eprintln!(
        "embedding cache: {} windows ({} dims each) built in {:.2}s ({:.1}x realtime)",
        cache.windows.len(),
        cache.embedding_dim,
        cache_elapsed.as_secs_f64(),
        audio_sec / cache_elapsed.as_secs_f64().max(1e-9)
    );

    let json = serde_json::to_string(&cache)?;
    std::fs::write(out, &json)?;
    eprintln!("wrote cache ({} bytes) to {}", json.len(), out.display());

    Ok(())
}

fn cmd_fast_recluster(
    cache_path: &std::path::Path,
    threshold: f64,
    max_gap_ms: u64,
    max_merged_ms: u64,
    format: OutputFormat,
    out: Option<&std::path::Path>,
) -> Result<()> {
    use asr_core::diarize::fast::{recluster, EmbeddingCache};
    use asr_core::diarize::merge_same_speaker_runs;

    let cache: EmbeddingCache = serde_json::from_str(
        &std::fs::read_to_string(cache_path).context("reading cache file")?,
    )
    .context("parsing cache file")?;

    let start = std::time::Instant::now();
    let raw_segments = recluster(&cache, threshold);
    let elapsed = start.elapsed();

    let merged_segments = merge_same_speaker_runs(&raw_segments, max_gap_ms, max_merged_ms);
    let distinct: std::collections::HashSet<i32> = merged_segments.iter().map(|s| s.speaker).collect();

    eprintln!(
        "recluster({} windows, threshold={threshold}): {:.2}ms, {} segments, {} distinct speakers",
        cache.windows.len(),
        elapsed.as_secs_f64() * 1000.0,
        merged_segments.len(),
        distinct.len()
    );

    let rendered = match format {
        OutputFormat::Json => serde_json::to_string_pretty(&merged_segments)?,
        OutputFormat::Rttm => {
            let uri = cache_path.file_stem().and_then(|s| s.to_str()).unwrap_or("cache");
            rttm::render(uri, &merged_segments)
        }
    };

    match out {
        Some(path) => {
            std::fs::write(path, &rendered)?;
            eprintln!("wrote {} segments to {}", merged_segments.len(), path.display());
        }
        None => println!("{rendered}"),
    }

    Ok(())
}

fn cmd_fast_sweep(cache_path: &std::path::Path, thresholds: &str) -> Result<()> {
    use asr_core::diarize::fast::{recluster, EmbeddingCache};
    use asr_core::diarize::merge_same_speaker_runs;

    let load_start = std::time::Instant::now();
    let cache: EmbeddingCache = serde_json::from_str(
        &std::fs::read_to_string(cache_path).context("reading cache file")?,
    )
    .context("parsing cache file")?;
    eprintln!(
        "loaded cache ({} windows) from disk in {:.2}ms — this I/O cost is separate from \
         re-clustering itself; a real app keeps the cache in memory",
        cache.windows.len(),
        load_start.elapsed().as_secs_f64() * 1000.0
    );

    println!("{:>10} {:>12} {:>10} {:>10}", "threshold", "recluster_ms", "segments", "speakers");
    for t in thresholds.split(',') {
        let threshold: f64 = t.trim().parse().with_context(|| format!("bad threshold '{t}'"))?;

        let start = std::time::Instant::now();
        let raw = recluster(&cache, threshold);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

        let merged = merge_same_speaker_runs(&raw, 500, 60_000);
        let distinct: std::collections::HashSet<i32> = merged.iter().map(|s| s.speaker).collect();

        println!("{threshold:>10} {elapsed_ms:>12.2} {:>10} {:>10}", merged.len(), distinct.len());
    }

    Ok(())
}

fn cmd_serve(cache_path: &std::path::Path, port: u16, max_gap_ms: u64, max_merged_ms: u64) -> Result<()> {
    use asr_core::diarize::fast::EmbeddingCache;

    let cache: EmbeddingCache = serde_json::from_str(
        &std::fs::read_to_string(cache_path).context("reading cache file")?,
    )
    .context("parsing cache file")?;

    serve::run(serve::ServeConfig { cache, max_gap_ms, max_merged_ms }, port)
}

fn cmd_transcribe(
    media: &std::path::Path,
    language: &str,
    api_key_env: &str,
    terms: Option<&str>,
) -> Result<()> {
    use asr_core::asr::{elevenlabs::ElevenLabsTranscriber, TranscribeRequest, Transcriber};

    let api_key = std::env::var(api_key_env)
        .with_context(|| format!("reading API key from ${api_key_env} (not set)"))?;

    let pcm = asr_core::audio::load_for_pipeline(media).context("decoding + resampling")?;
    let audio_sec = pcm.len() as f64 / asr_core::audio::TARGET_SAMPLE_RATE as f64;
    eprintln!("sending {audio_sec:.1}s of audio to ElevenLabs...");

    let transcriber = ElevenLabsTranscriber::new(api_key);
    let request = TranscribeRequest {
        audio: pcm,
        sample_rate: asr_core::audio::TARGET_SAMPLE_RATE,
        terms: terms
            .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default(),
        bias_strength: None,
        context_prompt: None,
        language: language.to_string(),
    };

    let start = std::time::Instant::now();
    let result = transcriber.transcribe(&request).context("ElevenLabs transcription failed")?;
    let elapsed = start.elapsed();

    println!("elapsed: {:.2}s", elapsed.as_secs_f64());
    println!("text:    {}", result.text);
    match &result.words {
        Some(words) => {
            println!("words:   {} (with timestamps + confidence)", words.len());
            for w in words.iter().take(10) {
                println!(
                    "  {:>7.1}-{:<7.1}  logprob={:>7.3}  {}",
                    w.start_ms as f64 / 1000.0,
                    w.end_ms as f64 / 1000.0,
                    w.confidence.unwrap_or(f32::NAN),
                    w.text
                );
            }
            if words.len() > 10 {
                println!("  ... ({} more)", words.len() - 10);
            }
        }
        None => println!("words:   (none returned)"),
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_transcribe_pipeline(
    media: &std::path::Path,
    models_dir: &std::path::Path,
    cluster_threshold: f32,
    max_gap_ms: u64,
    max_merged_ms: u64,
    pad_ms: u64,
    language: &str,
    api_key_env: &str,
    terms: Option<&str>,
    terms_db: Option<&std::path::Path>,
    project_id: &str,
    correction_threshold: f32,
    out: Option<&std::path::Path>,
) -> Result<()> {
    use asr_core::asr::elevenlabs::ElevenLabsTranscriber;
    use asr_core::diarize::{DiarizeModelPaths, DiarizeParams};
    use asr_core::terms::{self, store as terms_store};

    let api_key = std::env::var(api_key_env)
        .with_context(|| format!("reading API key from ${api_key_env} (not set)"))?;

    // Load the registry once, up front: it feeds both the keyterms sent
    // *before* ASR and the correction applied *after* it, and both need
    // the same terms/aliases to stay consistent with each other.
    let (db_terms, db_aliases) = match terms_db {
        Some(path) => {
            let conn = asr_core::store::open(path).context("opening terms DB")?;
            let db_terms = terms_store::load_terms(&conn, project_id).context("loading terms")?;
            let db_aliases = terms_store::load_aliases(&conn, project_id).context("loading term aliases")?;
            eprintln!("loaded {} terms, {} aliases from {}", db_terms.len(), db_aliases.len(), path.display());
            (db_terms, db_aliases)
        }
        None => (Vec::new(), Vec::new()),
    };

    let models = DiarizeModelPaths::under(models_dir);
    let params = DiarizeParams { cluster_threshold, ..Default::default() };

    eprintln!("--- stage 1/2: diarization ---");
    let decode_start = std::time::Instant::now();
    let pcm = asr_core::audio::load_for_pipeline(media).context("decoding + resampling")?;
    let decode_elapsed = decode_start.elapsed(); // measured before diarize starts, not after
    let diarize_start = std::time::Instant::now();
    let raw_segments = asr_core::diarize::diarize(&pcm, &models, &params).context("diarization failed")?;
    let merged_segments = asr_core::diarize::merge_same_speaker_runs(&raw_segments, max_gap_ms, max_merged_ms);
    eprintln!(
        "decoded in {:.1}s, diarized in {:.1}s -> {} merged segments",
        decode_elapsed.as_secs_f64(),
        diarize_start.elapsed().as_secs_f64(),
        merged_segments.len()
    );

    eprintln!("--- stage 2/2: transcription ({} sequential requests) ---", merged_segments.len());
    // `--terms-db` takes priority: it carries weight ordering (so the
    // highest-priority terms survive provider truncation) that a flat
    // `--terms` string can't express.
    let terms_vec: Vec<String> = if !db_terms.is_empty() {
        db_terms.iter().map(|t| t.surface.clone()).collect()
    } else {
        terms
            .map(|t| t.split(',').map(|s| s.trim().to_string()).collect())
            .unwrap_or_default()
    };

    let transcriber = ElevenLabsTranscriber::new(api_key);

    let transcribe_start = std::time::Instant::now();
    let mut transcript = asr_core::pipeline::run_transcription_stage(
        &pcm,
        &merged_segments,
        &transcriber,
        &terms_vec,
        language,
        pad_ms,
    )
    .context("transcription stage failed")?;
    eprintln!(
        "transcribed {} chunks in {:.1}s",
        transcript.len(),
        transcribe_start.elapsed().as_secs_f64()
    );

    if !db_terms.is_empty() {
        eprintln!("--- post-processing: terminology correction (layers 2+3) ---");
        // A fresh user dictionary from the current registry, so lindera's
        // readings for these exact terms are available (see the plan doc:
        // lindera returns UNK for OOV words, which registered technical
        // terms definitionally are).
        let dict_csv = std::env::temp_dir().join(format!("asr-cli-terms-{}.csv", std::process::id()));
        terms::build_user_dictionary_csv(&db_terms, &dict_csv).context("building user dictionary")?;
        let analyzer = terms::ReadingAnalyzer::new(Some(&dict_csv)).context("loading reading analyzer")?;
        let _ = std::fs::remove_file(&dict_csv); // best-effort; already loaded into memory

        for seg in &mut transcript {
            seg.text = terms::correct_text(&seg.text, &analyzer, &db_terms, &db_aliases, correction_threshold)
                .context("correcting segment text")?;
        }
    }

    let rendered = serde_json::to_string_pretty(&transcript)?;
    match out {
        Some(path) => {
            std::fs::write(path, &rendered)?;
            eprintln!("wrote transcript to {}", path.display());
        }
        None => {
            for seg in &transcript {
                println!(
                    "[{:>7.1}-{:<7.1}] speaker_{}: {}",
                    seg.start_ms as f64 / 1000.0,
                    seg.end_ms as f64 / 1000.0,
                    seg.speaker,
                    seg.text
                );
            }
        }
    }

    Ok(())
}

fn cmd_terms(action: TermsAction) -> Result<()> {
    use asr_core::terms::store as terms_store;
    use asr_core::terms::Term;

    match action {
        TermsAction::Add { db, project_id, surface, reading, weight, aliases } => {
            let conn = asr_core::store::open(&db).context("opening terms DB")?;
            conn.execute(
                "INSERT INTO projects (id, name) VALUES (?1, ?1) ON CONFLICT(id) DO NOTHING",
                [&project_id],
            )?;

            let term_id = terms_store::add_term(
                &conn,
                &project_id,
                &Term { surface: surface.clone(), reading_kana: reading.clone(), category: None, weight, enabled: true },
            )
            .context("adding term")?;

            for alias in &aliases {
                terms_store::add_alias(&conn, &term_id, alias).context("adding alias")?;
            }

            println!("registered term '{surface}' (id={term_id}) with {} alias(es)", aliases.len());
            Ok(())
        }
        TermsAction::List { db, project_id } => {
            let conn = asr_core::store::open(&db).context("opening terms DB")?;
            let terms = terms_store::load_terms(&conn, &project_id).context("loading terms")?;
            let aliases = terms_store::load_aliases(&conn, &project_id).context("loading aliases")?;

            for term in &terms {
                println!(
                    "{}  (reading={}, weight={})",
                    term.surface,
                    term.reading_kana.as_deref().unwrap_or("-"),
                    term.weight
                );
                for alias in aliases.iter().filter(|a| a.term_surface == term.surface) {
                    println!("  = {}", alias.alias_surface);
                }
            }
            if terms.is_empty() {
                println!("(no terms registered for project '{project_id}')");
            }
            Ok(())
        }
    }
}
