use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;
use tauri::Emitter;
use crate::binaries;
use crate::path_utils;

// Expose la dernière durée d'export terminée (en secondes)
static LAST_EXPORT_TIME_S: Mutex<Option<f64>> = Mutex::new(None);

// CONFIG DE DEVELOPPEMENT
// Mettre à `true` pour tester l'export CPU même si une carte Nvidia est dispo
// En PROD (release), cette valeur est ignorée et on privilégie toujours le GPU.
#[cfg(debug_assertions)]
const DEV_FORCE_CPU_ENCODING: bool = false;

fn should_prefer_hw_encoding() -> bool {
    #[cfg(debug_assertions)]
    {
        if DEV_FORCE_CPU_ENCODING {
            println!("[DEV] Forçage de l'encodage CPU activé (DEV_FORCE_CPU_ENCODING = true)");
            return false;
        }
    }
    true
}

// Gestionnaire des processus actifs pour pouvoir les annuler
static ACTIVE_EXPORTS: LazyLock<Mutex<HashMap<String, Arc<Mutex<Option<std::process::Child>>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// Structure pour gérer un export en flux direct (streaming)
struct StreamingSession {
    stdin: Arc<Mutex<std::process::ChildStdin>>,
}

// Gestionnaire des sessions de streaming actives
static ACTIVE_STREAMS: LazyLock<Mutex<HashMap<String, Arc<StreamingSession>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

// Fonction utilitaire pour configurer les commandes et cacher les fenêtres CMD sur Windows
fn configure_command_no_window(cmd: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x00004000;
        cmd.creation_flags(CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS);
    }
}

fn resolve_ffmpeg_binary() -> Option<String> {
    if let Some(path) = binaries::resolve_binary("ffmpeg") {
        return Some(path);
    }

    // En dernier recours, utiliser ffmpeg du PATH système
    println!("[ffmpeg] Tentative d'utilisation de ffmpeg du système (PATH)");
    if let Ok(_) = std::process::Command::new("ffmpeg").arg("-version").output() {
        println!("[ffmpeg] ✓ FFmpeg trouvé dans le PATH système");
        return Some("ffmpeg".to_string());
    }

    // Aucun binaire FFmpeg trouvé
    None
}

fn resolve_ffprobe_binary() -> String {
    if let Some(path) = binaries::resolve_binary("ffprobe") {
        return path;
    }

    // En dernier recours, utiliser ffprobe du PATH système
    println!("[ffprobe] Tentative d'utilisation de ffprobe du système (PATH)");
    if let Ok(_) = std::process::Command::new("ffprobe").arg("-version").output() {
        println!("[ffprobe] ✓ FFprobe trouvé dans le PATH système");
        return "ffprobe".to_string();
    }

    // Fallback vers le binaire système
    "ffprobe".to_string()
}

/// Teste si NVENC est réellement disponible en essayant un encodage rapide
fn test_nvenc_availability(ffmpeg_path: Option<&str>) -> bool {
    let exe = ffmpeg_path.unwrap_or("ffmpeg");
    
    println!("[nvenc_test] Test de disponibilité NVENC...");
    
    // Créer une entrée vidéo de test très courte (1 frame noir)
    // NVENC nécessite une résolution minimale (généralement 128x128 ou plus)
    let mut cmd = Command::new(exe);
    cmd.args(&[
        "-y",
        "-hide_banner",
        "-loglevel", "error",
        "-f", "lavfi",
        "-i", "color=c=black:s=128x128:r=1:d=0.04", // Résolution minimum NVENC, très courte
        "-c:v", "h264_nvenc",
        "-preset", "fast",
        "-pix_fmt", "yuv420p",
        "-frames:v", "1",
        "-f", "null", // Sortie nulle pour éviter d'écrire un fichier
        "-"
    ]);
    
    configure_command_no_window(&mut cmd);
    
    match cmd.output() {
        Ok(output) => {
            let success = output.status.success();
            let stderr = String::from_utf8_lossy(&output.stderr);
            
            if success {
                println!("[nvenc_test] ✓ NVENC disponible et fonctionnel");
                true
            } else {
                // Analyser les erreurs pour distinguer "pas disponible" vs "erreur de config"
                let stderr_lower = stderr.to_lowercase();
                
                if stderr_lower.contains("cannot load nvcuda.dll") || 
                   stderr_lower.contains("no nvidia devices") ||
                   stderr_lower.contains("cuda") ||
                   stderr_lower.contains("driver") {
                    println!("[nvenc_test] ✗ NVENC non disponible (pas de GPU NVIDIA ou drivers manquants)");
                    false
                } else if stderr_lower.contains("frame dimension") {
                    // Si c'est juste un problème de dimensions, essayer avec une plus grande résolution
                    println!("[nvenc_test] Retry avec résolution plus grande...");
                    test_nvenc_with_larger_resolution(ffmpeg_path)
                } else {
                    println!("[nvenc_test] ✗ NVENC erreur: {}", stderr.trim());
                    false
                }
            }
        }
        Err(e) => {
            println!("[nvenc_test] ✗ Erreur lors du test NVENC: {}", e);
            false
        }
    }
}

fn test_nvenc_with_larger_resolution(ffmpeg_path: Option<&str>) -> bool {
    let exe = ffmpeg_path.unwrap_or("ffmpeg");
    
    let mut cmd = Command::new(exe);
    cmd.args(&[
        "-y",
        "-hide_banner",
        "-loglevel", "error",
        "-f", "lavfi",
        "-i", "color=c=black:s=256x256:r=1:d=0.04", // Résolution encore plus grande
        "-c:v", "h264_nvenc",
        "-preset", "fast",
        "-pix_fmt", "yuv420p",
        "-frames:v", "1",
        "-f", "null",
        "-"
    ]);
    
    configure_command_no_window(&mut cmd);
    
    match cmd.output() {
        Ok(output) => {
            let success = output.status.success();
            if success {
                println!("[nvenc_test] ✓ NVENC disponible avec résolution 256x256");
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                println!("[nvenc_test] ✗ NVENC toujours non disponible: {}", stderr.trim());
            }
            success
        }
        Err(e) => {
            println!("[nvenc_test] ✗ Erreur test résolution plus grande: {}", e);
            false
        }
    }
}

fn probe_hw_encoders(ffmpeg_path: Option<&str>) -> Vec<String> {
    let exe = ffmpeg_path.unwrap_or("ffmpeg");
    
    let output = match Command::new(exe)
        .args(&["-hide_banner", "-encoders"])
        .output()
    {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    
    let txt = String::from_utf8_lossy(&output.stdout).to_lowercase();
    let mut found = Vec::new();
    
    if txt.contains("h264_nvenc") {
        found.push("h264_nvenc".to_string());
    }
    if txt.contains("h264_qsv") {
        found.push("h264_qsv".to_string());
    }
    if txt.contains("h264_amf") {
        found.push("h264_amf".to_string());
    }
    
    found
}

fn choose_best_codec(prefer_hw: bool) -> (String, Vec<String>, HashMap<String, Option<String>>) {
    let ffmpeg_exe = resolve_ffmpeg_binary();
    let hw = if prefer_hw {
        probe_hw_encoders(ffmpeg_exe.as_deref())
    } else {
        Vec::new()
    };
    
    if !hw.is_empty() {
        // Tester spécifiquement NVENC s'il est détecté
        if hw[0] == "h264_nvenc" {
            if test_nvenc_availability(ffmpeg_exe.as_deref()) {
                println!("[codec] Utilisation de NVENC (accélération GPU NVIDIA)");
                let codec = hw[0].clone();
                let params = vec![
                    "-pix_fmt".to_string(), "yuv420p".to_string(),
                    "-bf".to_string(), "0".to_string(),
                ];
                let mut extra = HashMap::new();
                extra.insert("preset".to_string(), Some("p4".to_string()));
                return (codec, params, extra);
            } else {
                println!("[codec] NVENC détecté mais non fonctionnel, fallback vers libx264");
            }
        } else {
            // Pour les autres encodeurs hardware (QSV, AMF), utiliser directement
            println!("[codec] Utilisation de l'encodeur hardware: {}", hw[0]);
            let codec = hw[0].clone();
            let params = vec!["-pix_fmt".to_string(), "yuv420p".to_string()];
            let mut extra = HashMap::new();
            extra.insert("preset".to_string(), None);
            return (codec, params, extra);
        }
    }
    
    // Fallback libx264
    println!("[codec] Utilisation de libx264 (encodage logiciel)");
    let codec = "libx264".to_string();
    let params = vec![
        "-pix_fmt".to_string(), "yuv420p".to_string(),
        "-crf".to_string(), "22".to_string(),
        "-tune".to_string(), "zerolatency".to_string(),
        "-bf".to_string(), "0".to_string(),
    ];
    let mut extra = HashMap::new();
    extra.insert("preset".to_string(), Some("ultrafast".to_string()));
    
    (codec, params, extra)
}

fn ffmpeg_preprocess_video(src: &str, dst: &str, w: i32, h: i32, fps: i32, prefer_hw: bool, start_ms: Option<i32>, duration_ms: Option<i32>, blur: Option<f64>) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let (codec, params, extra) = choose_best_codec(prefer_hw);
    let exe = resolve_ffmpeg_binary().unwrap_or_else(|| "ffmpeg".to_string());

    // Construire le filtre vidéo avec blur optionnel
    let mut vf_parts = vec![
        format!("scale=w={}:h={}:force_original_aspect_ratio=decrease", w, h),
        format!("pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black", w, h),
    ];
    
    // Ajouter le flou si spécifié et > 0
    if let Some(blur_value) = blur {
        if blur_value > 0.0 {
            vf_parts.push(format!("gblur=sigma={}", blur_value));
        }
    }
    
    vf_parts.push(format!("fps={}", fps));
    vf_parts.push("setsar=1".to_string());
    
    let vf = vf_parts.join(",");

    let mut cmd = Command::new(&exe);

    // Si un offset de début est fourni, l'ajouter avant -i pour seek rapide
    if let Some(sms) = start_ms {
        let s = format!("{:.3}", (sms as f64) / 1000.0);
        cmd.arg("-ss").arg(s);
    }

    cmd.arg("-y")
        .arg("-hide_banner")
        .arg("-loglevel").arg("error")
        .arg("-fflags").arg("+genpts")
        .arg("-avoid_negative_ts").arg("make_zero")
        .arg("-vsync").arg("cfr")
        .arg("-i").arg(src);

    // Si une durée de découpe est fournie, la limiter
    if let Some(dms) = duration_ms {
        let d = format!("{:.3}", (dms as f64) / 1000.0);
        cmd.arg("-t").arg(d);
    }

    let gop = fps * 2;
    cmd.arg("-an")
        .arg("-vf").arg(&vf)
        .arg("-pix_fmt").arg("yuv420p")
        .arg("-c:v").arg(&codec)
        .arg("-g").arg(gop.to_string());

    if let Some(Some(preset)) = extra.get("preset") {
        cmd.arg("-preset").arg(preset);
    }

    for param in params {
        cmd.arg(param);
    }

    cmd.arg(dst);

    // Configurer la commande pour cacher les fenêtres CMD sur Windows
    configure_command_no_window(&mut cmd);

    println!("[preproc] ffmpeg scale+pad -> {}", Path::new(dst).file_name().unwrap_or_default().to_string_lossy());

    let status = cmd.status()?;
    if !status.success() {
        return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "FFmpeg preprocessing failed")));
    }

    Ok(())
}

fn create_video_from_image(image_path: &str, output_path: &str, w: i32, h: i32, fps: i32, duration_s: f64, prefer_hw: bool, blur: Option<f64>) -> Result<(), Box<dyn std::error::Error>> {
    let ffmpeg_exe = resolve_ffmpeg_binary().unwrap_or_else(|| "ffmpeg".to_string());
    
    // Construire le filtre vidéo avec blur optionnel
    let mut vf_parts = vec![
        format!("scale={}:{}:force_original_aspect_ratio=increase", w, h),
        format!("crop={}:{}:(in_w-{})/2:(in_h-{})/2", w, h, w, h),
    ];
    
    // Ajouter le flou si spécifié et > 0
    if let Some(blur_value) = blur {
        if blur_value > 0.0 {
            vf_parts.push(format!("gblur=sigma={}", blur_value));
        }
    }
    
    let video_filter = vf_parts.join(",");
    
    // Choisir le meilleur codec avec détection automatique
    let (codec, codec_params, codec_extra) = choose_best_codec(prefer_hw);
    
    let mut cmd = Command::new(&ffmpeg_exe);
    cmd.args(&[
        "-y",
        "-hide_banner", 
        "-loglevel", "info",
        "-fflags", "+genpts",
        "-avoid_negative_ts", "make_zero",
        "-vsync", "cfr",
        "-loop", "1",
        "-i", image_path,
        "-vf", &video_filter,
        "-c:v", &codec,
        "-r", &fps.to_string(),
        "-g", &(fps * 2).to_string(),
        "-t", &format!("{:.6}", duration_s),
    ]);
    
    // Ajouter le preset si disponible
    if let Some(Some(preset)) = codec_extra.get("preset") {
        cmd.arg("-preset").arg(preset);
    }
    
    // Ajouter les paramètres du codec
    for param in codec_params {
        cmd.arg(param);
    }
    
    // Ajouter des paramètres de qualité selon le codec
    if codec == "libx264" {
        cmd.args(&["-crf", "23"]);
    } else if codec.contains("nvenc") {
        cmd.args(&["-cq", "23"]);
    }
    
    cmd.arg(output_path);

    // Configurer la commande pour cacher les fenêtres CMD sur Windows
    configure_command_no_window(&mut cmd);

    println!("[preproc][IMG] Création vidéo depuis image: {} -> {}", image_path, output_path);
    println!("[preproc][IMG] Commande: {:?}", cmd);

    let status = cmd.status()?;
    if !status.success() {
        return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "FFmpeg image-to-video failed")));
    }

    Ok(())
}

fn is_image_file(path: &str) -> bool {
    let path_lower = path.to_lowercase();
    path_lower.ends_with(".jpg") || path_lower.ends_with(".jpeg") || 
    path_lower.ends_with(".png") || path_lower.ends_with(".bmp") || 
    path_lower.ends_with(".gif") || path_lower.ends_with(".webp") ||
    path_lower.ends_with(".tiff") || path_lower.ends_with(".tif")
}

fn preprocess_background_videos(video_paths: &[String], w: i32, h: i32, fps: i32, prefer_hw: bool, start_time_ms: i32, duration_ms: Option<i32>, blur: Option<f64>) -> Vec<String> {
    println!("[preproc] Début du prétraitement pour {} vidéos/images...", video_paths.len());
    let mut out_paths = Vec::new();
    let cache_dir = std::env::temp_dir().join("qurancaption-preproc");
    fs::create_dir_all(&cache_dir).ok();

    // Cas spécial : une seule image
    if video_paths.len() == 1 && is_image_file(&video_paths[0]) {
        let image_path = &video_paths[0];
        let duration_s = if let Some(dur_ms) = duration_ms { 
            dur_ms as f64 / 1000.0 
        } else { 
            30.0 // Durée par défaut si non spécifiée
        };

        // Construire un nom de cache unique pour l'image
        let blur_suffix = if let Some(b) = blur {
            if b > 0.0 { format!("-blur{}", b) } else { String::new() }
        } else { String::new() };
        let hash_input = format!("{}-{}x{}-{}-dur{}{}-hw{}", image_path, w, h, fps, duration_s, blur_suffix, prefer_hw);
        let stem_hash = format!("{:x}", md5::compute(hash_input.as_bytes()));
        let stem_hash = &stem_hash[..10.min(stem_hash.len())];
        let dst = cache_dir.join(format!("img-bg-{}-{}x{}-{}.mp4", stem_hash, w, h, fps));

        if !dst.exists() {
            match create_video_from_image(image_path, &dst.to_string_lossy(), w, h, fps, duration_s, prefer_hw, blur) {
                Ok(_) => {},
                Err(e) => {
                    println!("[preproc][ERREUR] Impossible de créer la vidéo à partir de l'image: {:?}", e);
                    return vec![];
                }
            }
        }

        out_paths.push(dst.to_string_lossy().to_string());
        return out_paths;
    }

    // Calculer les durées (ms) de chaque vidéo
    let mut video_durations_ms: Vec<i64> = Vec::new();
    for p in video_paths {
        let d = (ffprobe_duration_sec(p) * 1000.0).round() as i64;
        video_durations_ms.push(d);
    }

    // Limite de la plage demandée
    let limit_ms: i64 = if let Some(dur) = duration_ms { dur as i64 } else { i64::MAX };

    // Parcourir les vidéos et extraire uniquement les segments pertinents
    let mut cum_start: i64 = 0;
    for (idx, p) in video_paths.iter().enumerate() {
        let vid_len = video_durations_ms.get(idx).cloned().unwrap_or(0);
        let cum_end = cum_start + vid_len;

        // Si la vidéo se termine avant le début recherché, on l'ignore complètement
        if cum_end <= start_time_ms as i64 {
            cum_start = cum_end;
            continue;
        }

        // Si on a déjà dépassé la limite demandée, on arrête
        let elapsed_so_far = cum_start - (start_time_ms as i64);
        if elapsed_so_far >= limit_ms {
            break;
        }

        // Déterminer le début à l'intérieur de cette vidéo
        let start_within = if start_time_ms as i64 > cum_start { start_time_ms as i64 - cum_start } else { 0 };

        // Durée restante à prendre dans cette vidéo
        let elapsed_from_start = (cum_start + start_within) - (start_time_ms as i64);
        let remaining_needed = (limit_ms - elapsed_from_start).max(0);
        let take_ms = remaining_needed.min(vid_len - start_within);

        if take_ms <= 0 {
            cum_start = cum_end;
            continue;
        }

        // Construire un nom de cache unique qui inclut les offsets et le blur
        let blur_suffix = if let Some(b) = blur {
            if b > 0.0 { format!("-blur{}", b) } else { String::new() }
        } else { String::new() };
        let hash_input = format!("{}-{}x{}-{}-start{}-len{}{}-hw{}", p, w, h, fps, start_within, take_ms, blur_suffix, prefer_hw);
        let stem_hash = format!("{:x}", md5::compute(hash_input.as_bytes()));
        let stem_hash = &stem_hash[..10.min(stem_hash.len())];
        let dst = cache_dir.join(format!("bg-{}-{}x{}-{}.mp4", stem_hash, w, h, fps));

        println!("[preproc] Traitement du segment {}/{} -> {:?}", idx + 1, video_paths.len(), dst.file_name());

        if !dst.exists() {
            // Appeler ffmpeg_preprocess_video avec les offsets locaux
            match ffmpeg_preprocess_video(p, &dst.to_string_lossy(), w, h, fps, prefer_hw, Some(start_within as i32), Some(take_ms as i32), blur) {
                Ok(_) => {},
                Err(e) => {
                    println!("[preproc][ERREUR] {:?}", e);
                    // En cas d'échec, utiliser la vidéo originale (et laisser ffmpeg final gérer le trim)
                    out_paths.push(p.clone());
                    cum_start = cum_end;
                    continue;
                }
            }
        }

        out_paths.push(dst.to_string_lossy().to_string());

        // Si on a atteint la limite, on arrête
        let elapsed_total = (cum_start + start_within + take_ms) - (start_time_ms as i64);
        if elapsed_total >= limit_ms {
            break;
        }

        cum_start = cum_end;
    }

    out_paths
}

fn ffprobe_duration_sec(path: &str) -> f64 {
    let exe = resolve_ffprobe_binary();
    
    let mut cmd = Command::new(&exe);
    cmd.args(&[
        "-v", "error",
        "-show_entries", "format=duration",
        "-of", "default=nokey=1:noprint_wrappers=1",
        path,
    ]);
    
    // Configurer la commande pour cacher les fenêtres CMD sur Windows
    configure_command_no_window(&mut cmd);
    
    let output = match cmd.output() {
        Ok(output) => output,
        Err(_) => return 0.0,
    };
    
    let txt = String::from_utf8_lossy(&output.stdout).trim().to_string();
    txt.parse::<f64>().unwrap_or(0.0)
}

fn video_has_audio(path: &str) -> bool {
    let exe = resolve_ffprobe_binary();

    let output = Command::new(&exe)
        .args(&[
            "-v", "error",
            "-select_streams", "a",
            "-show_entries", "stream=index",
            "-of", "csv=p=0",
            path,
        ])
        .output();

    match output {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    }
}

struct ExportTimings {
    durations_s: Vec<f64>,
    start_s: f64,
    duration_s: f64,
}

fn calculate_export_timings(
    timestamps_ms: &[i32],
    fps: i32,
    fade_duration_ms: i32,
    start_time_ms: i32,
    duration_ms: Option<i32>,
    is_high_fidelity: bool,
) -> ExportTimings {
    let n = timestamps_ms.len();
    let tail_ms = fade_duration_ms.max(1000);
    let frame_duration = 1.0 / (fps as f64);
    
    let snap_time = |ms: i32| -> f64 {
        let seconds = ms as f64 / 1000.0;
        let frames = (seconds / frame_duration).round();
        frames * frame_duration
    };

    let start_s = snap_time(start_time_ms);
    let end_ms = if let Some(dur_ms) = duration_ms {
        start_time_ms + dur_ms
    } else {
        timestamps_ms[n - 1] + tail_ms
    };
    let end_s = snap_time(end_ms);
    let duration_s_total = (end_s - start_s).max(frame_duration);
    
    let mut raw_durations = Vec::new();
    for i in 0..n {
        let t_curr = timestamps_ms[i];
        let t_next = if i < n - 1 { timestamps_ms[i + 1] } else { timestamps_ms[i] + tail_ms };
        let dur = (snap_time(t_next) - snap_time(t_curr)).max(0.001);
        raw_durations.push(dur);
    }

    let mut durations_s = Vec::new();
    if is_high_fidelity {
        durations_s = raw_durations;
    } else {
        // En mode Fast, on ne regroupe pas ici car build_filter_complex a besoin des labels individuels
        durations_s = raw_durations;
    }
    
    ExportTimings {
        durations_s,
        start_s,
        duration_s: duration_s_total,
    }
}

struct FilterContext {
    filter_complex: String,
    have_audio: bool,
    current_idx: i32,
    bg_start_idx: i32,
    audio_start_idx: i32,
    total_bg_s: f64,
}
fn build_filter_complex_content(
    w: i32,
    h: i32,
    fps: i32,
    fade_s: f64,
    n: usize,
    durations_s: &[f64],
    start_s: f64,
    duration_s: f64,
    pre_videos: &[String],
    audio_paths: &[String],
    audio_start_idx: i32,
    bg_start_idx: i32,
    current_idx: i32,
    is_streaming: bool,
    is_high_fidelity: bool,
) -> FilterContext {
    let mut filter_lines = Vec::new();
    let mut cur_idx = current_idx;

    let overlay_label = if is_streaming && is_high_fidelity {
        // Mode Linéaire (Fidélité Totale) : Le flux pipe contient déjà la séquence complète capturée à 30fps
        filter_lines.push(format!(
            "[0:v]format=rgba,scale=w={}:h={}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black@0,fps={},setpts=PTS-STARTPTS,setsar=1,format=yuva420p[lin_overlay]",
            w, h, w, h, fps
        ));
        "lin_overlay".to_string()
    } else {
        // Mode Rapide (Fade Linéaire) : Découpage intelligent par CLIPS logiques
        let mut split_outputs = String::new();
        for i in 0..n {
            split_outputs.push_str(&format!("[b{}]", i));
        }
        
        filter_lines.push(format!(
            "[0:v]format=rgba,scale=w={}:h={}:force_original_aspect_ratio=decrease,pad={}:{}:(ow-iw)/2:(oh-ih)/2:color=black@0,fps={},setpts=PTS-STARTPTS,setsar=1,format=yuva420p,split={}{}",
            w, h, w, h, fps, n, split_outputs
        ));
        
        // --- LOGIQUE DE GROUPEMENT PAR CLIPS ---
        // On regroupe les segments qui pointent sur les mêmes timings (identiques)
        struct GroupedClip {
            input_indices: Vec<usize>,
            pure_duration: f64,
            pipe_start: f64,
        }
        let mut groups: Vec<GroupedClip> = Vec::new();
        let mut current_pipe_pos = 0.0;
        
        for i in 0..n {
            let dur = durations_s[i];
            // Pour l'instant on garde simple: on ne regroupe que si c'est vraiment collé
            // Mais en streaming Fast, chaque index i est une image unique
            // Cependant, le frontend peut envoyer plusieurs fois vers des timings proches.
            // On va traiter chaque segment comme un clip pour FFmpeg MAIS on s'assure
            // que si le segment est la suite d'un silence ou d'un changement, FFmpeg gère.
            
            groups.push(GroupedClip {
                input_indices: vec![i],
                pure_duration: dur,
                pipe_start: current_pipe_pos,
            });
            current_pipe_pos += dur;
        }

        let mut concat_inputs = String::new();
        for (idx, group) in groups.iter().enumerate() {
            let s = group.pipe_start;
            let e = s + group.pure_duration;
            let d = group.pure_duration;
            
            // Sécurité fondu
            let safe_fade = fade_s.min(d / 2.0);
            let fade_out_start = (d - safe_fade).max(0.0);

            // On ne peut trimmer qu'un seul index b{} à la fois
            // Note: on utilise le premier index du groupe pour l'image source
            let src_idx = group.input_indices[0];

            filter_lines.push(format!(
                "[b{}]trim=start={:.6}:end={:.6},setpts=PTS-STARTPTS,fade=t=in:st=0:d={:.6}:alpha=1,fade=t=out:st={:.6}:d={:.6}:alpha=1[s{}]",
                src_idx, s, e, safe_fade, fade_out_start, safe_fade, idx
            ));
            
            concat_inputs.push_str(&format!("[s{}]", idx));
        }
        
        filter_lines.push(format!("{}concat=n={}:v=1:a=0[comp_overlay]", concat_inputs, groups.len()));
        "comp_overlay".to_string()
    };
    
    let mut total_bg_s = 0.0;
    for p in pre_videos {
        total_bg_s += ffprobe_duration_sec(p);
    }
    
    let bg_label = if pre_videos.is_empty() || total_bg_s <= 1e-6 {
        let color_full_idx = cur_idx;
        cur_idx += 1;
        // On ne peut pas mettre le -f lavfi ici, il sera ajouté dans le cmd builder
        format!("{}:v", color_full_idx)
    } else {
        let prev = if pre_videos.len() > 1 {
            let mut ins = String::new();
            for (i, _) in pre_videos.iter().enumerate() {
                ins.push_str(&format!("[{}:v]", bg_start_idx + i as i32));
            }
            filter_lines.push(format!("{}concat=n={}:v=1:a=0[bgcat]", ins, pre_videos.len()));
            "bgcat".to_string()
        } else {
            format!("{}:v", bg_start_idx)
        };
        
        filter_lines.push(format!("[{}]setpts=PTS-STARTPTS,setsar=1[bgtrim]", prev));
        let mut bl = "bgtrim".to_string();
        
        if total_bg_s + 1e-6 < duration_s {
            let remain = duration_s - total_bg_s;
            let color_pad_idx = cur_idx;
            cur_idx += 1;
            filter_lines.push(format!("[{}:v]setsar=1[colorpad]", color_pad_idx));
            filter_lines.push(format!("[bgtrim][colorpad]concat=n=2:v=1:a=0[bg]"));
            bl = "bg".to_string();
        }
        bl
    };
    
    filter_lines.push(format!("[{}][{}]overlay=shortest=1:x=0:y=0,format=yuv420p[vout]", bg_label, overlay_label));
    
    let mut total_audio_s = 0.0;
    for p in audio_paths {
        total_audio_s += ffprobe_duration_sec(p);
    }
    let have_audio = !audio_paths.is_empty() && start_s < total_audio_s - 1e-6;

    if have_audio {
        let a = audio_paths.len();
        if a == 1 {
            let a_idx = format!("{}:a", audio_start_idx);
            filter_lines.push(format!("[{}]aresample=48000[aa0]", a_idx));
            filter_lines.push(format!("[aa0]atrim=start={:.6},asetpts=PTS-STARTPTS,atrim=end={:.6}[aout]", start_s, duration_s));
        } else {
            for j in 0..a {
                let idx = audio_start_idx + j as i32;
                filter_lines.push(format!("[{}:a]aresample=48000[aa{}]", idx, j));
            }
            let mut ins = String::new();
            for j in 0..a {
                ins.push_str(&format!("[aa{}]", j));
            }
            filter_lines.push(format!("{}concat=n={}:v=0:a=1[aacat]", ins, a));
            filter_lines.push(format!("[aacat]atrim=start={:.6},asetpts=PTS-STARTPTS,atrim=end={:.6}[aout]", start_s, duration_s));
        }
    }
    
    FilterContext {
        filter_complex: filter_lines.join(";"),
        have_audio,
        current_idx: cur_idx,
        bg_start_idx,
        audio_start_idx,
        total_bg_s,
    }
}

#[allow(clippy::too_many_arguments)]
fn build_and_run_ffmpeg_filter_complex(
    export_id: &str,
    out_path: &str,
    image_paths: &[String],
    timestamps_ms: &[i32],
    target_size: (i32, i32),
    fps: i32,
    fade_duration_ms: i32,
    start_time_ms: i32,
    audio_paths: &[String],
    bg_videos: &[String],
    prefer_hw: bool,
    imgs_cwd: Option<&str>,
    duration_ms: Option<i32>,
    chunk_index: Option<i32>,
    blur: Option<f64>,
    app_handle: tauri::AppHandle,
) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    let (w, h) = target_size;
    let fade_s = (fade_duration_ms as f64 / 1000.0).max(0.0);
    
    let n = image_paths.len();
    if n == 0 {
        return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Aucune image fournie")));
    }
    
    let timings = calculate_export_timings(timestamps_ms, fps, fade_duration_ms, start_time_ms, duration_ms, false);
    let durations_s = timings.durations_s;
    let start_s = timings.start_s;
    let duration_s = timings.duration_s;

    let (vcodec, vparams, vextra) = choose_best_codec(prefer_hw);
    
    let mut pre_videos = Vec::new();
    if !bg_videos.is_empty() {
        pre_videos = preprocess_background_videos(bg_videos, w, h, fps, prefer_hw, start_time_ms, duration_ms, blur);
    }
    
    // Préparer le fichier concat
    let base_dir = if let Some(cwd) = imgs_cwd {
        PathBuf::from(cwd)
    } else {
        std::env::temp_dir()
    };
    fs::create_dir_all(&base_dir).ok();
    
    let concat_content = image_paths.join("|");
    let concat_hash = format!("{:x}", md5::compute(concat_content.as_bytes()));
    let concat_path = base_dir.join(format!("images-{}.ffconcat", &concat_hash[..8]));
    
    let mut concat_file = fs::File::create(&concat_path)?;
    writeln!(concat_file, "ffconcat version 1.0")?;
    for (i, p) in image_paths.iter().enumerate() {
        let escaped = path_utils::escape_ffconcat_path(p);
        writeln!(concat_file, "file '{}'", escaped)?;
        let duration_with_padding = durations_s[i] + fade_s;
        writeln!(concat_file, "duration {:.6}", duration_with_padding)?;
    }
    let escaped_last = path_utils::escape_ffconcat_path(&image_paths[n - 1]);
    writeln!(concat_file, "file '{}'", escaped_last)?;
    
    let mut cmd = Vec::new();
    let ffmpeg_exe = resolve_ffmpeg_binary().unwrap_or_else(|| "ffmpeg".to_string());
    cmd.extend_from_slice(&[
        ffmpeg_exe.clone(),
        "-y".to_string(),
        "-hide_banner".to_string(),
        "-loglevel".to_string(), "info".to_string(),
        "-fflags".to_string(), "+genpts".to_string(),
        "-avoid_negative_ts".to_string(), "make_zero".to_string(),
        "-vsync".to_string(), "cfr".to_string(),
        "-stats".to_string(),
        "-progress".to_string(), "pipe:2".to_string(),
    ]);
    
    let concat_name = concat_path.to_string_lossy().to_string();
    cmd.extend_from_slice(&[
        "-safe".to_string(), "0".to_string(),
        "-f".to_string(), "concat".to_string(),
        "-i".to_string(), concat_name,
    ]);
    
    let mut current_idx = 1;
    let bg_start_idx = current_idx;
    for p in &pre_videos {
        cmd.extend_from_slice(&["-i".to_string(), p.clone()]);
        current_idx += 1;
    }
    
    let audio_start_idx = current_idx;
    // On ne sait pas encore si on a de l'audio avant build_filter_complex_content
    // mais on ajoute les entrées quand même si audio_paths n'est pas vide
    if !audio_paths.is_empty() {
        for p in audio_paths {
            cmd.extend_from_slice(&["-i".to_string(), p.clone()]);
            current_idx += 1;
        }
    }

    let filter_ctx = build_filter_complex_content(
        w, h, fps, fade_s, n, &durations_s, start_s, duration_s, 
        &pre_videos, audio_paths, audio_start_idx, bg_start_idx, current_idx, false, false
    );
    
    let filter_complex = filter_ctx.filter_complex;
    let have_audio = filter_ctx.have_audio;
    let _final_idx = filter_ctx.current_idx;

    if pre_videos.is_empty() || filter_ctx.total_bg_s <= 1e-6 {
        cmd.extend_from_slice(&[
            "-f".to_string(), "lavfi".to_string(),
            "-i".to_string(), format!("color=c=black:s={}x{}:r={}:d={:.6}", w, h, fps, duration_s),
        ]);
    } else if filter_ctx.total_bg_s + 1e-6 < duration_s {
        let remain = duration_s - filter_ctx.total_bg_s;
        cmd.extend_from_slice(&[
            "-f".to_string(), "lavfi".to_string(),
            "-i".to_string(), format!("color=c=black:s={}x{}:r={}:d={:.6}", w, h, fps, remain),
        ]);
    }
    
    let tmp_dir = std::env::temp_dir();
    let fg_path = tmp_dir.join(format!("filter-{}.ffgraph", &format!("{:x}", md5::compute(filter_complex.as_bytes()))[..8]));
    fs::write(&fg_path, &filter_complex)?;
    
    cmd.extend_from_slice(&["-filter_complex_script".to_string(), fg_path.to_string_lossy().to_string()]);
    cmd.extend_from_slice(&["-map".to_string(), "[vout]".to_string()]);
    if have_audio {
        cmd.extend_from_slice(&["-map".to_string(), "[aout]".to_string()]);
    }
    
    // Codec vidéo + audio
    let gop = fps * 2;
    cmd.extend_from_slice(&[
        "-r".to_string(), fps.to_string(), 
        "-g".to_string(), gop.to_string(),
        "-c:v".to_string(), vcodec
    ]);
    if let Some(Some(preset)) = vextra.get("preset") {
        cmd.extend_from_slice(&["-preset".to_string(), preset.clone()]);
    }
    cmd.extend(vparams);
    
    if have_audio {
        // HYPOTHESE 1 : Si c'est un "Chunk" intermédiaire, on utilise du LOSSLESS (ALAC)
        // pour éviter la dégradation lors de la concaténation.
        // Si c'est un export final (direct), on utilise du AAC 320k standard.
        // ALAC est supporté dans le conteneur MP4/M4A.
        if chunk_index.is_some() {
            cmd.extend_from_slice(&[
                "-c:a".to_string(), "alac".to_string(), 
                "-ac".to_string(), "2".to_string()      // Force stéréo
            ]);
        } else {
            cmd.extend_from_slice(&[
                "-c:a".to_string(), "aac".to_string(), 
                "-b:a".to_string(), "320k".to_string(), // Qualité MAX pour éviter perte
                "-ac".to_string(), "2".to_string()      // Force stéréo
            ]);
        }
    }
    
    // Assure la durée exacte
    cmd.extend_from_slice(&["-t".to_string(), format!("{:.6}", duration_s)]);
    
    // Faststart pour formats MP4/MOV
    let ext = Path::new(out_path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    
    if matches!(ext.as_str(), "mp4" | "mov" | "m4v") {
        cmd.extend_from_slice(&["-movflags".to_string(), "+faststart".to_string()]);
    }
    
    // Fichier de sortie
    cmd.push(out_path.to_string());
    
    println!("[ffmpeg] Commande:");
    let preview = if cmd.len() > 14 {
        format!("{} ...", cmd[..14].join(" "))
    } else {
        cmd.join(" ")
    };
    println!("  {}", preview);
    
    // Exécution avec capture de la progression
    let mut command = Command::new(&cmd[0]);
    command.args(&cmd[1..]);
    command.stderr(Stdio::piped());
    
    // Configurer la commande pour cacher les fenêtres CMD sur Windows
    configure_command_no_window(&mut command);
    
    let child = command.spawn()?;
    
    // Enregistrer le processus dans les exports actifs
    let process_ref = Arc::new(Mutex::new(Some(child)));
    {
        let mut active_exports = ACTIVE_EXPORTS.lock().map_err(|_| "Failed to lock active exports")?;
        active_exports.insert(export_id.to_string(), process_ref.clone());
    }
    
    let stderr = {
        let mut child_guard = process_ref.lock().map_err(|_| "Failed to lock child process")?;
        if let Some(ref mut child) = child_guard.as_mut() {
            child.stderr.take().ok_or("Failed to capture stderr")?
        } else {
            return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, "Process was cancelled")));
        }
    };
    
    // Lire la sortie stderr pour capturer la progression
    let reader = BufReader::new(stderr);
    let mut stderr_content = String::new();
    
    for line in reader.lines() {
        if let Ok(line) = line {
            println!("[ffmpeg] {}", line); // Debug: afficher toutes les lignes
            
            // Sauvegarder toutes les lignes stderr pour le debugging
            stderr_content.push_str(&line);
            stderr_content.push('\n');
            
            // Chercher les lignes de progression FFmpeg qui contiennent "time=" ou "out_time_ms="
            if line.contains("time=") || line.contains("out_time_ms=") {
                if let Some(time_str) = extract_time_from_ffmpeg_line(&line) {
                    let current_time_s = parse_ffmpeg_time(&time_str);
                    let progress = if duration_s > 0.0 {
                        (current_time_s / duration_s * 100.0).min(100.0)
                    } else {
                        0.0
                    };
                    
                    println!("[progress] {}% ({:.1}s / {:.1}s)", progress.round(), current_time_s, duration_s);
                    
                    // Préparer les données de progression
                    let mut progress_data = serde_json::json!({
                        "export_id": export_id,
                        "progress": progress,
                        "current_time": current_time_s,
                        "total_time": duration_s
                    });
                    
                    // Ajouter chunk_index si fourni
                    if let Some(chunk_idx) = chunk_index {
                        progress_data["chunk_index"] = serde_json::Value::Number(serde_json::Number::from(chunk_idx));
                    }
                    
                    // Émettre l'événement de progression vers le frontend
                    let _ = app_handle.emit("export-progress", progress_data);
                }
            }
        }
    }
    
    // Attendre la fin du processus
    let status = {
        let mut child_guard = process_ref.lock().map_err(|_| "Failed to lock child process")?;
        if let Some(mut child) = child_guard.take() {
            child.wait()?
        } else {
            // Le processus a été annulé
            let error_msg = format!("Export {} was cancelled", export_id);
            let mut error_data = serde_json::json!({
                "export_id": export_id,
                "error": error_msg
            });
            
            // Ajouter chunk_index si fourni
            if let Some(chunk_idx) = chunk_index {
                error_data["chunk_index"] = serde_json::Value::Number(serde_json::Number::from(chunk_idx));
            }
            
            let _ = app_handle.emit("export-error", error_data);
            return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Interrupted, error_msg)));
        }
    };
    
    // Nettoyer les exports actifs
    {
        let mut active_exports = ACTIVE_EXPORTS.lock().map_err(|_| "Failed to lock active exports")?;
        active_exports.remove(export_id);
    }
    
    if !status.success() {
        // Créer un fichier de log avec la date d'aujourd'hui
        let now = std::time::SystemTime::now();
        let timestamp = now.duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let log_filename = format!("ffmpeg_failed_{}.txt", timestamp);
        
        let log_content = format!(
            "FFmpeg Export Failure Log\n\
             =========================\n\
             Timestamp: {}\n\
             Export ID: {}\n\
             Exit Code: {:?}\n\
             \n\
             FFmpeg Command:\n\
             {}\n\
             \n\
             Standard Error Output:\n\
             {}\n",
            timestamp,
            export_id,
            status.code(),
            cmd.join(" "),
            if stderr_content.is_empty() {
                "No stderr output captured".to_string()
            } else {
                stderr_content
            }
        );
        
        // Écrire le fichier de log
        if let Err(log_err) = std::fs::write(&log_filename, &log_content) {
            eprintln!("Failed to write log file {}: {}", log_filename, log_err);
        } else {
            println!("FFmpeg error details saved to: {}", log_filename);
        }
        
        let error_msg = format!(
            "ffmpeg failed during video exportation (exit code: {:?})\n\nSee the log file: {}\n\nLog details:\n{}", 
            status.code(), 
            log_filename,
            log_content
        );
        let mut error_data = serde_json::json!({
            "export_id": export_id,
            "error": error_msg
        });
        
        // Ajouter chunk_index si fourni
        if let Some(chunk_idx) = chunk_index {
            error_data["chunk_index"] = serde_json::Value::Number(serde_json::Number::from(chunk_idx));
        }
        
        let _ = app_handle.emit("export-error", error_data);
        return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, error_msg)));
    }
    
    Ok(())
}

#[tauri::command]
pub async fn export_video(
    export_id: String,
    imgs_folder: String,
    final_file_path: String,
    fps: i32,
    fade_duration: i32,
    start_time: i32,
    duration: Option<i32>,
    audios: Option<Vec<String>>,
    videos: Option<Vec<String>>,
    chunk_index: Option<i32>,
    blur: Option<f64>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let t0 = Instant::now();
    
    // Logs init
    println!("[start_export] export_id={}", export_id);
    println!("[start_export] imgs_folder={}", imgs_folder);
    println!("[start_export] final_file_path={}", final_file_path);
    println!("[start_export] fps={}, fade_duration(ms)={}", fps, fade_duration);
    println!("[env] CPU cores: {:?}", std::thread::available_parallelism().map(|n| n.get()));
    
    if let Some(ref audios) = audios {
        println!("[audio] {} fichier(s) audio fourni(s)", audios.len());
    } else {
        println!("[audio] aucun fichier audio fourni");
    }
    
    if let Some(ref videos) = videos {
        println!("[video] {} fichier(s) vidéo fourni(s)", videos.len());
    } else {
        println!("[video] aucune vidéo de fond fournie");
    }
    
    // Liste des PNG triés par timestamp
    let folder = path_utils::normalize_existing_path(&imgs_folder);
    println!("[scan] Parcours du dossier: {:?}", folder.canonicalize().unwrap_or_else(|_| folder.clone()));
    
    let mut files: Vec<_> = fs::read_dir(&folder)
        .map_err(|e| format!("Erreur lecture dossier: {}", e))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()?.to_str()?.to_lowercase() == "png" {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    
    files.sort_by_key(|p| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i32>().ok())
            .unwrap_or(0)
    });
    let files: Vec<PathBuf> = files
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect();
    
    println!("[scan] {} image(s) trouvée(s)", files.len());
    
    if files.is_empty() {
        return Err("Aucune image .png trouvée dans imgs_folder".to_string());
    }
    
    let _first_stem = files[0]
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(-1);
    

    
    // Timeline et chemins
    let ts: Vec<i32> = files
        .iter()
        .map(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<i32>().ok())
                .unwrap_or(0)
        })
        .collect();
    
    let path_strs: Vec<String> = files
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    
    let ts_preview: Vec<i32> = ts.iter().take(10).cloned().collect();
    println!("[timeline] Premiers timestamps: {:?}{}", ts_preview, if ts.len() > 10 { " ..." } else { "" });
    println!("[timeline] Nombre d'images: {}", ts.len());
    
    // Taille cible = taille de 0.png
    println!("[image] Ouverture de la première image pour taille cible...");
    let target_size = {
        let img_data = fs::read(&files[0]).map_err(|e| format!("Erreur lecture image: {}", e))?;
        let img = image::load_from_memory(&img_data).map_err(|e| format!("Erreur décodage image: {}", e))?;
        (img.width() as i32, img.height() as i32)
    };
    
    println!("[image] Taille cible: {}x{}", target_size.0, target_size.1);
    
    // Durée totale
    let fade_ms = fade_duration;
    let tail_ms = fade_ms.max(1000);
    let total_duration_ms = ts[ts.len() - 1] + tail_ms;
    let duration_s = total_duration_ms as f64 / 1000.0;
    println!("[timeline] Durée totale: {} ms ({:.3} s)", total_duration_ms, duration_s);
    println!("[perf] Préparation terminée en {:.0} ms", t0.elapsed().as_millis());
    
    let out_path = path_utils::normalize_output_path(&final_file_path);
    if let Some(parent) = out_path.parent() {
        println!("[fs] Création du dossier de sortie si besoin: {:?}", parent);
        fs::create_dir_all(parent).map_err(|e| format!("Erreur création dossier: {}", e))?;
    }
    
    let imgs_folder_resolved = folder.canonicalize()
        .unwrap_or_else(|_| folder.clone())
        .to_string_lossy()
        .to_string();
    
    let out_path_str = out_path.to_string_lossy().to_string();
    let out_path_str_for_task = out_path_str.clone();
    let audios_vec: Vec<String> = audios
        .unwrap_or_default()
        .into_iter()
        .map(|p| path_utils::normalize_existing_path(&p).to_string_lossy().to_string())
        .collect();
    let videos_vec: Vec<String> = videos
        .unwrap_or_default()
        .into_iter()
        .map(|p| path_utils::normalize_existing_path(&p).to_string_lossy().to_string())
        .collect();
    let app_handle = app.clone();
    let export_id_clone = export_id.clone();
    
    start_streaming_export(
        export_id.clone(),
        out_path_str_for_task,
        imgs_folder_resolved,
        ts,
        target_size,
        fps,
        fade_ms,
        start_time,
        audios_vec,
        videos_vec,
        should_prefer_hw_encoding(),
        duration,
        chunk_index,
        blur,
        true, // is_high_fidelity
        app.clone(),
    ).await.map_err(|e| format!("WGPU Export error: {}", e))?;
    
    let export_time_s = t0.elapsed().as_secs_f64();
    *LAST_EXPORT_TIME_S.lock().unwrap() = Some(export_time_s);
    println!("[done] Export terminé en {:.2}s", export_time_s);
    println!("[metric] export_time_seconds={:.3}", export_time_s);
    
    // Extraire le nom de fichier de sortie
    let output_file_name = out_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    
    // Préparer les données de completion
    let mut completion_data = serde_json::json!({
        "filename": output_file_name,
        "exportId": export_id,
        "fullPath": out_path_str
    });
    
    // Ajouter chunk_index si fourni
    if let Some(chunk_idx) = chunk_index {
        completion_data["chunkIndex"] = serde_json::Value::Number(serde_json::Number::from(chunk_idx));
    }
    
    // Émettre l'événement de succès
    let _ = app.emit("export-complete", completion_data);
    
    Ok(out_path_str)
}

// Fonctions utilitaires pour parser la progression FFmpeg
fn extract_time_from_ffmpeg_line(line: &str) -> Option<String> {
    // Chercher "time=" dans la ligne et extraire la valeur
    if let Some(start) = line.find("time=") {
        let start = start + 5; // Longueur de "time="
        if let Some(end) = line[start..].find(char::is_whitespace) {
            return Some(line[start..start + end].to_string());
        } else {
            // Si pas d'espace trouvé, prendre jusqu'à la fin
            return Some(line[start..].to_string());
        }
    }
    
    // Aussi chercher le format "out_time_ms=" pour -progress pipe
    if let Some(start) = line.find("out_time_ms=") {
        let start = start + 12; // Longueur de "out_time_ms="
        if let Some(end) = line[start..].find(char::is_whitespace) {
            if let Ok(ms) = line[start..start + end].parse::<i64>() {
                let seconds = ms as f64 / 1_000_000.0; // microseconds to seconds
                return Some(format!("{:.3}", seconds));
            }
        }
    }
    
    None
}

fn parse_ffmpeg_time(time_str: &str) -> f64 {
    // Si c'est déjà en secondes (format décimal)
    if let Ok(seconds) = time_str.parse::<f64>() {
        return seconds;
    }
    
    // Format FFmpeg : HH:MM:SS.mmm
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() == 3 {
        if let (Ok(hours), Ok(minutes), Ok(seconds)) = 
            (parts[0].parse::<f64>(), parts[1].parse::<f64>(), parts[2].parse::<f64>()) {
            return hours * 3600.0 + minutes * 60.0 + seconds;
        }
    }
    0.0
}

#[tauri::command]
pub async fn cancel_export(export_id: String) -> Result<String, String> {
    println!("[cancel_export] Demande d'annulation pour export_id: {}", export_id);

    // 1. Fermer le flux de streaming si il existe
    {
        let mut streams = ACTIVE_STREAMS.lock().map_err(|e| e.to_string())?;
        if let Some(_session) = streams.remove(&export_id) {
            println!("[cancel_export] Fermeture du flux stdin pour {}", export_id);
            // session est retiré de la map et sera droppé à la fin de ce bloc,
            // ce qui fermera le stdin si c'était la dernière référence.
        }
    }

    // 2. Tuer le processus
    let mut active_exports = ACTIVE_EXPORTS.lock().map_err(|_| "Failed to lock active exports")?;
    if let Some(process_ref) = active_exports.remove(&export_id) {
        println!("[cancel_export] Found active process for {}, locking...", export_id);
        let mut child_guard = process_ref.lock().unwrap();
        if let Some(mut child) = child_guard.take() {
            println!("[cancel_export] Suppression forcée du processus FFmpeg {}", export_id);
            let _ = child.kill();
            let _ = child.wait(); // Nettoyer
            Ok(format!("Export {} annulé avec succès", export_id))
        } else {
            println!("[cancel_export] Processus déjà terminé ou pris par un autre fil pour {}", export_id);
            Ok(format!("Export {} déjà terminé", export_id))
        }
    } else {
        println!("[cancel_export] Export_id non trouvé dans les exports actifs: {}", export_id);
        Err(format!("Export {} non trouvé ou déjà terminé", export_id))
    }
}

#[tauri::command]
pub async fn concat_videos(
    export_id: String,
    video_paths: Vec<String>,
    output_path: String,
) -> Result<String, String> {
    let normalized_video_paths: Vec<String> = video_paths
        .into_iter()
        .map(|p| path_utils::normalize_existing_path(&p).to_string_lossy().to_string())
        .collect();
    let output_path_buf = path_utils::normalize_output_path(&output_path);
    let output_path_str = output_path_buf.to_string_lossy().to_string();

    println!("[concat_videos] Début de la concaténation de {} vidéos", normalized_video_paths.len());
    println!("[concat_videos] Fichier de sortie: {}", output_path_str);
    
    if normalized_video_paths.is_empty() {
        return Err("Aucune vidéo fournie pour la concaténation".to_string());
    }
    
    if normalized_video_paths.len() == 1 {
        // Si une seule vidéo, on peut simplement la copier ou la renommer
        println!("[concat_videos] Une seule vidéo, copie vers le fichier final");
        std::fs::copy(&normalized_video_paths[0], &output_path_str)
            .map_err(|e| format!("Erreur lors de la copie: {}", e))?;
        return Ok(output_path_str);
    }
    
    // Créer le dossier de sortie si nécessaire
    if let Some(parent) = output_path_buf.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Erreur création dossier de sortie: {}", e))?;
    }
    
    // Créer un fichier de liste temporaire pour FFmpeg
    let temp_dir = std::env::temp_dir();
    let list_file_path = temp_dir.join(format!("concat_list_{}.txt", 
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()));
    
    // Écrire la liste des fichiers à concaténer
    let mut list_content = String::new();
    for video_path in &normalized_video_paths {
        // Vérifier que le fichier existe
        if !Path::new(video_path).exists() {
            return Err(format!("Fichier vidéo non trouvé: {}", video_path));
        }
        let escaped = path_utils::escape_ffconcat_path(video_path);
        list_content.push_str(&format!("file '{}'\n", escaped));
    }
    
    fs::write(&list_file_path, list_content)
        .map_err(|e| format!("Erreur écriture fichier liste: {}", e))?;
    
    println!("[concat_videos] Fichier liste créé: {:?}", list_file_path);
    
    // Préparer la commande FFmpeg
    let ffmpeg_exe = resolve_ffmpeg_binary().unwrap_or_else(|| "ffmpeg".to_string());
    
    let mut cmd = Command::new(&ffmpeg_exe);
    cmd.args(&[
        "-y",                           // Écraser le fichier de sortie
        "-hide_banner",                 // Masquer le banner FFmpeg
        "-loglevel", "info",            // Niveau de log
        "-fflags", "+genpts",           // Régénère les pts pour éviter les gaps
        "-f", "concat",                 // Format d'entrée concat
        "-safe", "0",                   // Permettre les chemins absolus
        "-i", &list_file_path.to_string_lossy(), // Fichier de liste
        "-avoid_negative_ts", "make_zero", // Normalise les timestamps
        "-map", "0:v",                  // Vidéo
        "-c:v", "copy",                 // Pas de ré-encodage vidéo
    ]);

    // Ré-encoder l'audio pour lisser les timestamps et éviter les micro-cuts
    if normalized_video_paths.iter().any(|p| video_has_audio(p)) {
        cmd.args(&[
            "-map", "0:a?",                          // Map audio si présent (sans échouer si absent)
            "-af", "aresample=async=1:first_pts=0",  // Corrige les horloges audio
            "-c:a", "aac",
            "-b:a", "320k",                          // Qualité MAX
            "-ac", "2",                              // Force stéréo
        ]);
    } else {
        cmd.arg("-an"); // Aucun audio trouvé, on désactive l'audio
    }

    cmd.arg(&output_path_str);                  // Fichier de sortie
    
    // Configurer la commande pour cacher les fenêtres CMD sur Windows
    configure_command_no_window(&mut cmd);
    
    println!("[concat_videos] Exécution de FFmpeg...");
    
    // Lancement du processus en mode Child pour pouvoir l'annuler
    let mut child = cmd.spawn()
        .map_err(|e| format!("Erreur lancement FFmpeg concat: {}", e))?;
    
    // Enregistrement dans ACTIVE_EXPORTS
    let process_ref = Arc::new(Mutex::new(Some(child)));
    {
        let mut active_exports = ACTIVE_EXPORTS.lock().map_err(|_| "Failed to lock active exports")?;
        active_exports.insert(export_id.clone(), process_ref.clone());
        println!("[concat_videos] Process registered in ACTIVE_EXPORTS with ID: {}", export_id);
    }

    // Attente de la fin du processus
    let wait_result = {
        // On clone la ref pour attendre sans bloquer le lock global ACTIVE_EXPORTS trop longtemps si on devait le garder
        // Mais ici on a besoin de lock le process_ref specific
        let mut loop_count = 0;
        loop {
            // On vérifie si annulé
            {
                let mut guard = process_ref.lock().unwrap();
                if guard.is_none() {
                    println!("[concat_videos] Process cancellation detected for {}", export_id);
                    // Processus annulé et take() par cancel_export
                    let _ = fs::remove_file(&list_file_path);
                    return Err("Concaténation annulée par l'utilisateur".to_string());
                }
                
                // Vérifier si fini sans bloquer indéfiniment (polling)
                match guard.as_mut().unwrap().try_wait() {
                    Ok(Some(status)) => {
                        println!("[concat_videos] Process finished with status: {:?}", status);
                        break Ok(status)
                    },
                    Ok(None) => {
                        loop_count += 1;
                        if loop_count % 10 == 0 { // Log every 5s
                             println!("[concat_videos] Still running... ({}s)", (loop_count as f64) * 0.5);
                        }
                    }, 
                    Err(e) => {
                        println!("[concat_videos] Error polling process: {}", e);
                        break Err(e)
                    },
                }
            }
            // Petit sleep pour ne pas burn le CPU
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    };

    // Nettoyage de ACTIVE_EXPORTS
    {
        let mut active_exports = ACTIVE_EXPORTS.lock().unwrap();
        active_exports.remove(&export_id);
    }
    
    // Nettoyer le fichier temporaire
    let _ = fs::remove_file(&list_file_path);
    
    match wait_result {
        Ok(status) => {
            if !status.success() {
                return Err(format!("FFmpeg concat a échoué avec le code {:?}", status.code()));
            }
        },
        Err(e) => return Err(format!("Erreur attente FFmpeg concat: {}", e)),
    }
    
    // Vérifier que le fichier de sortie a été créé
    if !Path::new(&output_path_str).exists() {
        return Err("Le fichier de sortie n'a pas été créé".to_string());
    }
    
    println!("[concat_videos] ✅ Concaténation réussie: {}", output_path_str);
    Ok(output_path_str)
}

#[tauri::command]
pub async fn start_streaming_export(
    export_id: String,
    out_path: String,
    imgs_folder: String,
    timestamps_ms: Vec<i32>,
    target_size: (i32, i32),
    fps: i32,
    fade_duration_ms: i32,
    start_time_ms: i32,
    audio_paths: Vec<String>,
    bg_videos: Vec<String>,
    prefer_hw: bool,
    duration_ms: Option<i32>,
    chunk_index: Option<i32>,
    blur: Option<f64>,
    is_high_fidelity: bool,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let (w, h) = target_size;
    let fade_s = (fade_duration_ms as f64 / 1000.0).max(0.0);
    // --- WGPU MIGRATION ---
    // We ignore most of the complex filter logic and use our Rust Renderer.
    // However, we still need to respect the interface.
    
    // 1. Setup Renderer
    let mut renderer = crate::renderer::Renderer::new(w as u32, h as u32).await.map_err(|e| e.to_string())?;
    
    // 2. Setup Video Decoder (Background)
    // For now we assume the first background video is the main one. 
    // If multiple, we would need a playlist logic in Decoder.
    let bg_path = if !bg_videos.is_empty() {
        &bg_videos[0]
    } else {
        return Err("No background video provided".to_string());
    };
    
    let mut decoder = crate::renderer::VideoDecoder::new(bg_path, w as u32, h as u32, fps as u32)
        .map_err(|e| format!("Decoder error: {}", e))?;
        
    // 3. Setup Video Encoder (Output) avec codec et audio
    let (vcodec, vparams, vextra) = choose_best_codec(prefer_hw);
    let vpreset = vextra.get("preset").and_then(|p| p.clone());
    
    let duration_s = duration_ms.unwrap_or(0) as f64 / 1000.0;
    let start_s = start_time_ms as f64 / 1000.0;

    let mut encoder = crate::renderer::VideoEncoder::new(
        &out_path, 
        w as u32, 
        h as u32, 
        fps as u32,
        &vcodec,
        vparams,
        vpreset,
        &audio_paths,
        start_s,
        duration_s
    ).map_err(|e| format!("Encoder error: {}", e))?;
        
    // 4. Register Encoder Child for Cancellation
    // The encoder.child is the one writing the file, so we track it.
    {
         // Small hack: we can't easily clone the child, but we can wrap it if we change the struct.
         // For now, let's just assume we don't track it in ACTIVE_EXPORTS directly *here* 
         // because VideoEncoder owns it. 
         // TODO: Refactor ACTIVE_EXPORTS to hold a CancellationHandle instead of Child process.
         // For this MVP, if user cancels, we might need a way to stop this loop.
         // We will check a cancellation flag in the loop?
    }
    
    let total_frames = if let Some(d) = duration_ms {
        (d as f64 / 1000.0 * fps as f64) as usize
    } else {
        // Fallback or calc from timings
        100 // dummy
    };

    let start_inst = std::time::Instant::now();

    // 5. Render Loop
    // Running in a separate task to avoid blocking the main thread? 
    // Current function is async, so we can just run loop and await.
    // But VideoEncoder/Decoder are blocking IO for now. Ideally wrap in spawn_blocking.
    
    let export_id_clone = export_id.clone();
    let app_handle_clone = app_handle.clone();
    
    tokio::task::spawn_blocking(move || {
        let mut frame_idx = 0;
        let mut loop_err = None;
        let mut last_sub_idx: Option<usize> = None;
        
        loop {
            if frame_idx >= total_frames {
                break;
            }
            
            // 1. Decode Frame
            let bg_data = match decoder.read_frame() {
                Ok(d) => d,
                Err(e) => {
                    if e == "EOF" { break; }
                    loop_err = Some(e);
                    break;
                }
            };
            
            // 2. Upload to GPU
            renderer.upload_background(&bg_data);
            
            // 3. Render Subtitle Overlay
            let time_ms = (frame_idx as f64 / fps as f64 * 1000.0) as i32 + start_time_ms;
            
            // Find current subtitle
            let mut current_sub_idx = None;
            for (i, &ts) in timestamps_ms.iter().enumerate() {
                let end = if i + 1 < timestamps_ms.len() { timestamps_ms[i+1] } else { i32::MAX };
                if time_ms >= ts && time_ms < end {
                    current_sub_idx = Some(i);
                    break;
                }
            }

            if let Some(idx) = current_sub_idx {
                // Load and upload subtitle texture if changed
                if last_sub_idx != Some(idx) {
                    let sub_path = PathBuf::from(&imgs_folder).join(format!("{}.png", idx));
                    if sub_path.exists() {
                        if let Ok(img_data) = std::fs::read(sub_path) {
                            if let Ok(img) = image::load_from_memory(&img_data) {
                                let rgba = img.to_rgba8();
                                renderer.upload_subtitle(&rgba);
                            }
                        }
                    }
                    last_sub_idx = Some(idx);
                }

                // Calculate Alpha for Fade
                let start_ms = timestamps_ms[idx];
                let end_ms = if idx + 1 < timestamps_ms.len() { timestamps_ms[idx+1] } else { timestamps_ms[idx] + 2000 };
                
                let mut alpha = 1.0f32;
                let rel_ms = time_ms - start_ms;
                let rel_end_ms = end_ms - time_ms;
                
                if rel_ms < fade_duration_ms {
                    alpha = (rel_ms as f32 / fade_duration_ms as f32).min(1.0);
                } else if rel_end_ms < fade_duration_ms {
                    alpha = (rel_end_ms as f32 / fade_duration_ms as f32).min(1.0);
                }

                renderer.render_image(alpha);
            } else {
                last_sub_idx = None;
            }
            
            // 4. Readback
            let frame_out = tokio::runtime::Handle::current().block_on(renderer.read_frame());
            let frame_out = match frame_out {
                Ok(f) => f,
                Err(e) => {
                    loop_err = Some(e);
                    break;
                }
            };
            
            // 5. Encode
            if let Err(e) = encoder.write_frame(&frame_out) {
                loop_err = Some(e);
                break;
            }
            
            // Progress
            if frame_idx % 30 == 0 {
                  let _ = app_handle_clone.emit("export-progress", serde_json::json!({
                    "export_id": export_id_clone,
                    "progress": (frame_idx as f64 / total_frames as f64) * 100.0,
                }));
            }
            
            frame_idx += 1;
        }
        
        if let Some(e) = loop_err {
            let _ = app_handle_clone.emit("export-error", serde_json::json!({ "error": e }));
        } else {
             if let Err(e) = encoder.finish() {
                 let _ = app_handle_clone.emit("export-error", serde_json::json!({ "error": e }));
             } else {
                 let _ = app_handle_clone.emit("export-complete", serde_json::json!({ "filename": out_path }));
             }
        }
        
    }).await.map_err(|e| e.to_string())?;

    Ok(())
}

// Stub for unused functions keeping interface
#[tauri::command]
pub async fn send_frame(_export_id: String, _frame_data: Vec<u8>, _count: u32) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
pub async fn finish_streaming_export(_export_id: String) -> Result<(), String> {
    Ok(())
}
