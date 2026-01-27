<script lang="ts">
	import { PredefinedSubtitleClip, VerseRange, type AssetClip } from '$lib/classes';
	import Timeline from '$lib/components/projectEditor/timeline/Timeline.svelte';
	import VideoPreview from '$lib/components/projectEditor/videoPreview/VideoPreview.svelte';
	import { globalState } from '$lib/runes/main.svelte';
	import { ProjectService } from '$lib/services/ProjectService';
	import { invoke } from '@tauri-apps/api/core';
	import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
	import { listen } from '@tauri-apps/api/event';
	import { onMount } from 'svelte';
	import { exists, BaseDirectory, mkdir, writeFile, remove, readFile } from '@tauri-apps/plugin-fs';
	import { LogicalPosition } from '@tauri-apps/api/dpi';
	import { getCurrentWebview } from '@tauri-apps/api/webview';
	import { appDataDir, join } from '@tauri-apps/api/path';
	import ExportService, { type ExportProgress } from '$lib/services/ExportService';
	import { getAllWindows } from '@tauri-apps/api/window';
	import Exportation, { ExportState } from '$lib/classes/Exportation.svelte';
	import toast from 'svelte-5-french-toast';
	import DomToImage from 'dom-to-image';
	import SubtitleClip from '$lib/components/projectEditor/timeline/track/SubtitleClip.svelte';
	import { ClipWithTranslation, CustomTextClip, SilenceClip } from '$lib/classes/Clip.svelte';

	// Indique si l'enregistrement a commencé
	let readyToExport = $state(false);

	// Contient l'ID de l'export
	let exportId = '';

	// VideoPreview
	let videoPreview: VideoPreview | undefined = $state(undefined);

	// Récupère les données d'export de la vidéo
	let exportData: Exportation | undefined;

	// Durée de chunk calculée dynamiquement basée sur chunkSize (1-200)
	// chunkSize = 1 -> 30s, chunkSize = 50 -> 2min30, chunkSize = 200 -> 10min
	let CHUNK_DURATION = 0; // Sera calculé dans onMount

	async function exportProgress(event: any) {
		const data = event.payload as {
			progress?: number;
			current_time: number;
			total_time?: number;
			export_id: string;
			chunk_index?: number;
		};

		// Vérifie que c'est bien pour cette exportation
		if (data.export_id !== exportId) return;

		if (data.progress !== null && data.progress !== undefined) {
			console.log(
				`Export Progress: ${data.progress.toFixed(1)}% (${data.current_time.toFixed(1)}s / ${data.total_time?.toFixed(1)}s)`
			);

			const chunkIndex = data.chunk_index || 0;
			const totalDuration = exportData!.videoEndTime - exportData!.videoStartTime;
			const totalChunks = Math.ceil(totalDuration / CHUNK_DURATION);

			// Calculer le pourcentage global et le temps actuel global
			let globalProgress: number;
			let globalCurrentTime: number;

			if (data.chunk_index !== undefined) {
				// Mode chunked export
				// Chaque chunk représente une portion égale du pourcentage total
				// Calcul donc le pourcentage global basé sur le chunk actuel et son progrès
				const chunkProgressWeight = 100 / totalChunks;
				const baseProgress = chunkIndex * chunkProgressWeight;
				const chunkLocalProgress = (data.progress / 100) * chunkProgressWeight;
				globalProgress = baseProgress + chunkLocalProgress;

				// Calculer le temps global basé sur la position du chunk et son progrès
				const chunkDuration = Math.min(CHUNK_DURATION, totalDuration - chunkIndex * CHUNK_DURATION);
				const chunkLocalTime = (data.current_time / (data.total_time || 1)) * chunkDuration;
				globalCurrentTime = chunkIndex * CHUNK_DURATION + chunkLocalTime;
			} else {
				// Mode export normal (sans chunks)
				globalProgress = data.progress;
				globalCurrentTime = data.current_time * 1000; // Convertir de secondes en millisecondes
			}

			emitProgress({
				exportId: Number(exportId),
				progress: globalProgress,
				currentState: ExportState.CreatingVideo,
				currentTime: globalCurrentTime
			} as ExportProgress);
		} else {
			console.log(`Export Processing: ${data.current_time.toFixed(1)}s elapsed`);
		}
	}

	async function exportComplete(event: any) {
		const data = event.payload as { filename: string; exportId: string; chunkIndex?: number };

		// Vérifie que c'est bien pour cette exportation
		if (data.exportId !== exportId) return;

		console.log(`✅ Export complete! File saved as: ${data.filename}`);

		// Si c'est un chunk, ne pas émettre 100% maintenant (ça sera fait à la fin de tous les chunks)
		if (data.chunkIndex === undefined) {
			// Export normal (sans chunks) - émettre 100%
			await emitProgress({
				exportId: Number(exportId),
				progress: 100,
				currentState: ExportState.Exported
			} as ExportProgress);
		} else {
			// Export en chunks - juste logger la completion du chunk
			console.log(`✅ Chunk ${data.chunkIndex} completed`);
		}
	}

	async function exportError(event: any) {
		const error = event.payload as { error: string; export_id: string };
		console.error(`❌ Export failed: ${error}`);

		if (error.export_id !== exportId) return;

		emitProgress({
			exportId: Number(exportId),
			progress: 100,
			currentState: ExportState.Error,
			errorLog: error.error
		} as ExportProgress);
	}

	async function emitProgress(progress: ExportProgress) {
		(await getAllWindows()).find((w) => w.label === 'main')!.emit('export-progress-main', progress);
	}

	onMount(async () => {
		// Écoute les événements de progression d'export donné par Rust
		listen('export-progress', exportProgress);
		listen('export-complete', exportComplete);
		listen('export-error', exportError);

		// Récupère l'id de l'export, qui est en paramètre d'URL
		const id = new URLSearchParams(window.location.search).get('id');
		if (id) {
			exportId = id;

			// Récupère le projet correspondant à cette ID (dans le dossier export, paramètre inExportFolder: true)
			globalState.currentProject = await ExportService.loadProject(Number(id));

			// Créer le dossier d'export s'il n'existe pas
			await mkdir(await join(ExportService.exportFolder, exportId), {
				baseDir: BaseDirectory.AppData,
				recursive: true
			});

			// Supprime le fichier projet JSON
			ExportService.deleteProjectFile(Number(id));

			// Récupère les données d'export
			exportData = ExportService.findExportById(Number(id))!;

			// Prépare les paramètres pour exporter la vidéo
			globalState.getVideoPreviewState.isFullscreen = true; // Met la vidéo en plein écran
			globalState.getVideoPreviewState.isPlaying = false; // Met la vidéo en pause
			globalState.getVideoPreviewState.showVideosAndAudios = true; // Met la vidéo en sourdine
			// Met le curseur au début du startTime voulu pour l'export
			globalState.getTimelineState.cursorPosition = globalState.getExportState.videoStartTime;
			globalState.getTimelineState.movePreviewTo = globalState.getExportState.videoStartTime;
			// Hide waveform: consomme des ressources inutilement
			if (globalState.settings) globalState.settings.persistentUiState.showWaveforms = false;
			// Plus besoin de diviser par 2, car le backend gère maintenant correctement le timing
			globalState.getStyle('global', 'fade-duration')!.value = globalState.getStyle(
				'global',
				'fade-duration'
			)!.value as number;

			const chunkSize = globalState.getExportState.chunkSize;
			// Formule linéaire: chunkSize=1 -> 10s, chunkSize=200 -> 20s
			// Ces valeurs sont réduites pour éviter la saturation mémoire (crash FFmpeg)
			const minDuration = 10 * 1000; // 10 secondes en ms
			const maxDuration = 20 * 1000; // 20 secondes en ms
			CHUNK_DURATION = Math.round(
				minDuration + ((chunkSize - 1) / (200 - 1)) * (maxDuration - minDuration)
			);

			console.log(
				`Chunk size: ${chunkSize}, Chunk duration: ${CHUNK_DURATION}ms (${CHUNK_DURATION / 1000}s)`
			);

			// Enlève tout les styles de position de la vidéo
			let videoElement: HTMLElement;
			// Attend que l'élément soit prêt
			do {
				await new Promise((resolve) => setTimeout(resolve, 100));
				videoElement = document.getElementById('video-preview-section') as HTMLElement;
				videoElement.style.objectFit = 'contain';
				videoElement.style.top = '0';
				videoElement.style.left = '0';
				videoElement.style.width = '100%';
				videoElement.style.height = '100%';
			} while (!videoElement);

			// Attend 2 secondes que tout soit prêt
			await new Promise((resolve) => setTimeout(resolve, 2000));

			readyToExport = true;

			// Démarrer l'export
			await startExport();
		}
	});

	async function startExport() {
		if (!exportData) return;

		const exportStart = Math.round(exportData.videoStartTime);
		const exportEnd = Math.round(exportData.videoEndTime);
		const totalDuration = exportEnd - exportStart;

		console.log(`Export duration: ${totalDuration}ms (${totalDuration / 1000 / 60} minutes)`);

		// Détection de la complexité pour ajuster la stratégie
		const isHighFidelity = globalState.getCustomClipTrack?.clips.length > 0;
		// Si FastMode, on peut faire de très gros chunks (5min). Si HighFi, on reste prudent (15s).
		const DYNAMIC_CHUNK_DURATION = isHighFidelity ? 15000 : 300000;

		// Si la durée est supérieure à la durée idéale d'un chunk, on découpe
		if (totalDuration > DYNAMIC_CHUNK_DURATION) {
			console.log(
				`Duration > ${DYNAMIC_CHUNK_DURATION}ms, using chunked export (HiFi: ${isHighFidelity})`
			);
			await handleChunkedExport(exportStart, exportEnd, totalDuration, isHighFidelity);
		} else {
			console.log('Duration short, using normal export');
			await handleNormalExport(exportStart, exportEnd, totalDuration);
		}
	}

	async function handleChunkedExport(
		exportStart: number,
		exportEnd: number,
		totalDuration: number,
		isHighFidelity: boolean
	) {
		const chunkInfo = calculateChunksWithFadeOut(exportStart, exportEnd, isHighFidelity);
		const generatedVideoFiles: string[] = [];

		console.log(`Splitting into ${chunkInfo.chunks.length} chunks`);

		// Initialiser l'état
		emitProgress({
			exportId: Number(exportId),
			progress: 0,
			currentState: ExportState.Initializing,
			currentTime: 0,
			totalTime: totalDuration
		} as ExportProgress);

		for (let i = 0; i < chunkInfo.chunks.length; i++) {
			const chunk = chunkInfo.chunks[i];
			const chunkDuration = chunk.end - chunk.start;
			const nextProgressWeight = 100 / chunkInfo.chunks.length;
			const baseProgress = i * nextProgressWeight;

			console.log(`Processing Chunk ${i}: ${chunk.start} -> ${chunk.end}`);

			// 1. Démarrer FFmpeg pour ce chunk
			const audios = globalState.getAudioTrack.clips.map(
				(clip: any) => globalState.currentProject!.content.getAssetById(clip.assetId).filePath
			);
			const videos = globalState.getVideoTrack.clips.map(
				(clip: any) => globalState.currentProject!.content.getAssetById(clip.assetId).filePath
			);
			const chunkVideoFileName = `chunk_${i}_video.mp4`;
			const chunkFinalFilePath = await join(
				await appDataDir(),
				ExportService.exportFolder,
				exportId,
				chunkVideoFileName
			);

			const hasCustomClips =
				globalState.getCustomClipTrack?.clips.some((c: any) => {
					return c.startTime! < chunk.end && c.endTime! > chunk.start;
				}) || false;

			const timings = calculateTimingsForRange(chunk.start, chunk.end, !hasCustomClips);

			// Si Fast Mode, les timings sont simplifiés (start/end), ce qui génère 1 segment par clip -> 1 fade in/out

			try {
				await invoke('start_streaming_export', {
					exportId: exportId,
					outPath: chunkFinalFilePath,
					timestampsMs: timings.uniqueSorted,
					targetSize: [exportData!.videoDimensions.width, exportData!.videoDimensions.height],
					fps: exportData!.fps,
					fadeDurationMs: Math.round(
						globalState.getStyle('global', 'fade-duration')!.value as number
					),
					startTimeMs: Math.round(chunk.start),
					audioPaths: audios,
					bgVideos: videos,
					preferHw: true,
					durationMs: Math.round(chunkDuration),
					chunkIndex: i,
					blur: globalState.getStyle('global', 'overlay-blur')!.value as number,
					overlayColor: globalState.getStyle('global', 'overlay-color')!.value as string,
					overlayOpacity:
						(globalState.getStyle('global', 'overlay-opacity')!.value as number) / 100,
					overlayEnable: globalState.getStyle('global', 'overlay-enable')!.value as boolean,
					isHighFidelity: hasCustomClips
				});
			} catch (e: any) {
				console.error('Error starting export chunk:', e);
				emitProgress({
					exportId: Number(exportId),
					progress: 100,
					currentState: ExportState.Error,
					errorLog: JSON.stringify(e, Object.getOwnPropertyNames(e))
				} as ExportProgress);
				throw e;
			}

			// 2. Diffuser les images
			await streamFramesForChunk(
				i,
				chunk.start,
				chunk.end,
				timings,
				baseProgress,
				baseProgress + nextProgressWeight,
				hasCustomClips
			);

			// 3. Finaliser le chunk
			await invoke('finish_streaming_export', { exportId: exportId });
			generatedVideoFiles.push(chunkFinalFilePath);
			console.log(`✅ Chunk ${i} generated via streaming`);
		}

		// PHASE FINALE: Concaténation
		console.log('=== FINAL PHASE: Concatenation ===');
		try {
			await concatenateVideos(generatedVideoFiles);
		} catch (e) {
			console.error('Concatenation failed:', e);
			throw e;
		}

		emitProgress({
			exportId: Number(exportId),
			progress: 100,
			currentState: ExportState.Exported,
			currentTime: totalDuration,
			totalTime: totalDuration
		} as ExportProgress);

		await finalCleanup();
	}

	async function streamFramesForChunk(
		chunkIndex: number | null,
		chunkStart: number,
		chunkEnd: number,
		timings: any,
		phaseStartProgress: number,
		phaseEndProgress: number,
		isHighFidelity: boolean
	) {
		console.log(
			`[Stream] Processing chunk ${chunkIndex} (Mode: ${isHighFidelity ? 'HighFidelity' : 'Fast'})`
		);
		const fps = exportData!.fps;
		const frame_duration_ms = 1000.0 / fps;

		const totalFramesExpected = Math.round(((chunkEnd - chunkStart) / 1000.0) * fps);
		let framesSent = 0;

		if (!isHighFidelity) {
			// ================= MODE RAPIDE (FFmpeg Fades) =================
			// On force l'opacité 100% car le backend applique les fondus
			globalState.exportFullOpacity = true;

			for (let i = 0; i < timings.uniqueSorted.length; i++) {
				const timing = timings.uniqueSorted[i];
				const t_next = i < timings.uniqueSorted.length - 1 ? timings.uniqueSorted[i + 1] : chunkEnd;

				const dur_ms = Math.max(t_next - timing, 1);
				const count = Math.round((dur_ms / 1000.0) * fps);

				if (count > 0) {
					// Capture au milieu du segment pour une image stable
					const capturePoint = timing + dur_ms / 2;
					globalState.getTimelineState.movePreviewTo = capturePoint;
					globalState.getTimelineState.cursorPosition = capturePoint;
					await wait(capturePoint);

					const bytes = await captureFrameRaw();
					if (bytes) {
						await invoke('send_frame', { exportId: exportId, frameData: bytes, count: count });
						framesSent += count;
					}
				}

				const progress =
					phaseStartProgress +
					(i / timings.uniqueSorted.length) * (phaseEndProgress - phaseStartProgress);
				emitProgress({
					exportId: Number(exportId),
					progress: progress,
					currentState: ExportState.CapturingFrames,
					currentTime: timing - exportData!.videoStartTime,
					totalTime: exportData!.videoEndTime - exportData!.videoStartTime
				} as ExportProgress);
			}
			globalState.exportFullOpacity = false;
		} else {
			// ================= MODE HAUTE FIDÉLITÉ (Captures 30fps) =================
			let currentTime = chunkStart;
			const endTime = chunkEnd;
			globalState.exportFullOpacity = false;

			while (currentTime < endTime && framesSent < totalFramesExpected) {
				const nextTiming = timings.uniqueSorted.find((t: number) => t > currentTime + 1) || endTime;

				// Vérifier si nous sommes dans une zone de transition (fondus)
				const isInTransition = timings.transitionZones.some(
					(zone: any) => currentTime < zone.end && currentTime >= zone.start - 1
				);

				const isStaticZone = !isInTransition && nextTiming - currentTime > frame_duration_ms * 1.5;

				if (isStaticZone) {
					// ZONE STATIQUE : Capture 1 image et répétition
					const durationToNext = nextTiming - currentTime;
					let framesToRepeat = Math.floor(durationToNext / frame_duration_ms);
					framesToRepeat = Math.min(framesToRepeat, totalFramesExpected - framesSent);

					if (framesToRepeat > 0) {
						// On se place au milieu du segment statique pour une capture propre
						const capturePoint = currentTime + durationToNext / 2;
						globalState.getTimelineState.movePreviewTo = capturePoint;
						globalState.getTimelineState.cursorPosition = capturePoint;
						await wait(capturePoint);

						const bytes = await captureFrameRaw();
						if (bytes) {
							await invoke('send_frame', {
								exportId: exportId,
								frameData: bytes,
								count: framesToRepeat
							});
							framesSent += framesToRepeat;
						}
						currentTime += framesToRepeat * frame_duration_ms;
					} else {
						currentTime += frame_duration_ms;
					}
				} else {
					// ZONE DE TRANSITION : Capture photo par photo à 30fps
					globalState.getTimelineState.movePreviewTo = currentTime;
					globalState.getTimelineState.cursorPosition = currentTime;
					await wait(currentTime);

					const bytes = await captureFrameRaw();
					if (bytes) {
						await invoke('send_frame', { exportId: exportId, frameData: bytes, count: 1 });
						framesSent++;
					}
					currentTime += frame_duration_ms;
				}

				// Progrès
				const progress =
					phaseStartProgress +
					(framesSent / totalFramesExpected) * (phaseEndProgress - phaseStartProgress);
				emitProgress({
					exportId: Number(exportId),
					progress: progress,
					currentState: ExportState.CapturingFrames,
					currentTime: currentTime - exportData!.videoStartTime,
					totalTime: exportData!.videoEndTime - exportData!.videoStartTime
				} as ExportProgress);
			}
		}

		console.log(`[Stream] Chunk ${chunkIndex} done. Total frames sent: ${framesSent}`);
	}

	async function concatenateVideos(videoFilePaths: string[]) {
		console.log('Starting video concatenation...');

		try {
			const finalVideoPath = await invoke('concat_videos', {
				exportId: exportId,
				videoPaths: videoFilePaths,
				outputPath: exportData!.finalFilePath
			});

			console.log('✅ Videos concatenated successfully:', finalVideoPath);

			// Supprimer les vidéos de chunks individuelles
			for (const videoPath of videoFilePaths) {
				try {
					await remove(videoPath, { baseDir: BaseDirectory.AppData });
					console.log(`Deleted chunk video: ${videoPath}`);
				} catch (e) {
					console.warn(`Could not delete chunk video ${videoPath}:`, e);
				}
			}
		} catch (e: any) {
			console.error('❌ Error concatenating videos:', e);
			emitProgress({
				exportId: Number(exportId),
				progress: 100,
				currentState: ExportState.Error,
				errorLog: JSON.stringify(e, Object.getOwnPropertyNames(e))
			} as ExportProgress);
			throw e;
		}
	}

	function hasTiming(
		blankImgs: { [surah: number]: number[] },
		t: number
	): {
		hasIt: boolean;
		surah: number | null;
	} {
		for (const [surahNumb, _timings] of Object.entries(blankImgs)) {
			if (_timings.includes(t)) return { hasIt: true, surah: Number(surahNumb) };
		}
		return { hasIt: false, surah: null };
	}

	function hasBlankImg(imgWithNothingShown: { [surah: number]: number }, surah: number): boolean {
		return imgWithNothingShown[surah] !== undefined;
	}

	async function handleNormalExport(exportStart: number, exportEnd: number, totalDuration: number) {
		// Détection Custom Clips
		const hasCustomClips = globalState.getCustomClipTrack?.clips.length > 0;
		const timings = calculateTimingsForRange(exportStart, exportEnd, !hasCustomClips);

		const audios = globalState.getAudioTrack.clips.map(
			(clip: any) => globalState.currentProject!.content.getAssetById(clip.assetId).filePath
		);
		const videos = globalState.getVideoTrack.clips.map(
			(clip: any) => globalState.currentProject!.content.getAssetById(clip.assetId).filePath
		);

		emitProgress({
			exportId: Number(exportId),
			progress: 0,
			currentState: ExportState.Initializing,
			currentTime: 0,
			totalTime: totalDuration
		} as ExportProgress);

		try {
			await invoke('start_streaming_export', {
				exportId: exportId,
				outPath: exportData!.finalFilePath,
				timestampsMs: timings.uniqueSorted,
				targetSize: [exportData!.videoDimensions.width, exportData!.videoDimensions.height],
				fps: exportData!.fps,
				fadeDurationMs: Math.round(
					globalState.getStyle('global', 'fade-duration')!.value as number
				),
				startTimeMs: Math.round(exportStart),
				audioPaths: audios,
				bgVideos: videos,
				preferHw: true,
				durationMs: Math.round(totalDuration),
				chunkIndex: null,
				blur: globalState.getStyle('global', 'overlay-blur')!.value as number,
				isHighFidelity: globalState.getCustomClipTrack?.clips.length > 0
			});
		} catch (e: any) {
			console.error('Error starting normal export:', e);
			emitProgress({
				exportId: Number(exportId),
				progress: 100,
				currentState: ExportState.Error,
				errorLog: JSON.stringify(e, Object.getOwnPropertyNames(e))
			} as ExportProgress);
			throw e;
		}

		await streamFramesForChunk(
			null,
			exportStart,
			exportEnd,
			timings,
			0,
			100,
			globalState.getCustomClipTrack?.clips.length > 0
		);

		await invoke('finish_streaming_export', { exportId: exportId });

		emitProgress({
			exportId: Number(exportId),
			progress: 100,
			currentState: ExportState.Exported,
			currentTime: totalDuration,
			totalTime: totalDuration
		} as ExportProgress);

		await finalCleanup();
	}

	async function finalCleanup() {
		try {
			await remove(await join(ExportService.exportFolder, exportId), {
				baseDir: BaseDirectory.AppData,
				recursive: true
			});
			console.log('Temporary images folder removed.');
		} catch (e) {
			console.warn('Could not remove temporary folder:', e);
		}
		getCurrentWebviewWindow().close();
	}

	async function captureFrameRaw(): Promise<Uint8Array | null> {
		console.log('[Capture] Starting captureFrameRaw...');
		let node = document.getElementById('overlay')!;
		if (!node) {
			console.error('[Capture] Overlay node not found');
			return null;
		}
		const targetWidth = exportData!.videoDimensions.width;
		const targetHeight = exportData!.videoDimensions.height;
		const scale = Math.min(targetWidth / node.clientWidth, targetHeight / node.clientHeight);
		try {
			const dataUrl = (await Promise.race([
				DomToImage.toPng(node, {
					width: node.clientWidth * scale,
					height: node.clientHeight * scale,
					style: { transform: 'scale(' + scale + ')', transformOrigin: 'top left' },
                    filter: (node: Node) => {
                        return (node as Element).id !== 'overlay-tint-layer';
                    }
				}),
				new Promise((_, reject) =>
					setTimeout(() => reject(new Error('DomToImage timeout (10s)')), 10000)
				)
			])) as string;
			console.log('[Capture] DomToImage success');
			const response = await fetch(dataUrl);
            const arrayBuffer = await response.arrayBuffer();
			const bytes = new Uint8Array(arrayBuffer);
            if (bytes.length < 1000) console.warn(`[Capture] Suspiciously small frame: ${bytes.length} bytes`);
			return bytes;
		} catch (error) {
			console.error('[Capture] Error while capturing frame: ', error);
			return null;
		}
	}

	function getCustomClipStateAt(timing: number): string {
		const visibleCustomClips: string[] = [];
		for (const ctClip of globalState.getCustomClipTrack?.clips || []) {
			const category = (ctClip as any).category;
			if (!category) continue;
			const alwaysShow = (category.getStyle('always-show')?.value as number) || 0;
			if (alwaysShow) continue;
			const startTime = category.getStyle('time-appearance')?.value as number;
			const endTime = category.getStyle('time-disappearance')?.value as number;
			if (startTime == null || endTime == null) continue;
			if (timing >= startTime && timing <= endTime) {
				visibleCustomClips.push(`${ctClip.id}-${startTime}-${endTime}`);
			}
		}
		return visibleCustomClips.sort().join('|');
	}

	function calculateTimingsForRange(
		rangeStart: number,
		rangeEnd: number,
		fastMode: boolean = false
	) {
		const fadeDuration = Math.round(
			globalState.getStyle('global', 'fade-duration')!.value as number
		);
		let timingsToTakeScreenshots: number[] = [Math.round(rangeStart), Math.round(rangeEnd)];
		let imgWithNothingShown: { [surah: number]: number } = {};
		let blankImgs: { [surah: number]: number[] } = {};
		let duplicableTimings: Map<number, number> = new Map();
		let transitionZones: Array<{ start: number; end: number }> = [];

		function add(t: number | undefined) {
			if (t === undefined) return;
			if (t < rangeStart || t > rangeEnd) return;
			timingsToTakeScreenshots.push(Math.round(t));
		}

		function addTransition(s: number, e: number) {
			if (e <= s) return;
			if (e < rangeStart || s > rangeEnd) return;
			transitionZones.push({ start: Math.max(rangeStart, s), end: Math.min(rangeEnd, e) });
		}

		for (const clip of globalState.getSubtitleTrack.clips) {
			const { startTime, endTime } = clip as any;
			if (startTime == null || endTime == null) continue;
			if (endTime < rangeStart || startTime > rangeEnd) continue;
			const duration = endTime - startTime;
			if (duration <= 0) continue;

			if (!(clip instanceof SilenceClip) && clip.type !== 'Silence') {
				if (fastMode) {
					// En mode rapide, on ne veut QUE le début et la fin pour avoir 1 seul segment
					add(startTime);
					add(endTime);
				} else {
					const fadeInEnd = Math.min(startTime + fadeDuration, endTime);
					const fadeOutStart = endTime - fadeDuration;

					add(startTime);
					add(fadeInEnd);
					addTransition(startTime, fadeInEnd);

					if (fadeOutStart > startTime) {
						add(fadeOutStart);
						add(endTime);
						addTransition(fadeOutStart, endTime);

						if (fadeInEnd < fadeOutStart) {
							if (getCustomClipStateAt(fadeInEnd) === getCustomClipStateAt(fadeOutStart)) {
								duplicableTimings.set(Math.round(fadeOutStart), Math.round(fadeInEnd));
							}
						}
					} else {
						add(endTime);
					}
				}
			} else {
				const surah = globalState.getSubtitleTrack.getCurrentSurah(clip.startTime);
				if (imgWithNothingShown[surah] === undefined) {
					add(endTime);
				} else {
					if (!blankImgs[surah]) blankImgs[surah] = [];
					blankImgs[surah].push(Math.round(endTime));
				}
			}
		}

		for (const ctClip of globalState.getCustomClipTrack?.clips || []) {
			const category = (ctClip as any).category;
			if (!category) continue;
			const alwaysShow = (category.getStyle('always-show')?.value as number) || 0;
			const startTime = category.getStyle('time-appearance')?.value as number;
			const endTime = category.getStyle('time-disappearance')?.value as number;
			if (startTime == null || endTime == null) continue;
			if (endTime < rangeStart || startTime > rangeEnd) continue;

			if (alwaysShow) {
				add(startTime);
				add(endTime);
				continue;
			}

			const ctFadeInEnd = Math.min(startTime + fadeDuration, endTime);
			add(startTime);
			add(ctFadeInEnd);
			addTransition(startTime, ctFadeInEnd);

			const ctFadeOutStart = endTime - fadeDuration;
			if (ctFadeOutStart > startTime) {
				add(ctFadeOutStart);
				add(endTime);
				addTransition(ctFadeOutStart, endTime);
			} else {
				add(endTime);
			}
		}

		const uniqueSorted = Array.from(new Set(timingsToTakeScreenshots))
			.filter((t) => t >= rangeStart && t <= rangeEnd)
			.sort((a, b) => a - b);

		return { uniqueSorted, imgWithNothingShown, blankImgs, duplicableTimings, transitionZones };
	}

	function calculateChunksWithFadeOut(
		exportStart: number,
		exportEnd: number,
		isHighFidelity: boolean
	) {
		// En High Fidelity (30fps), on garde des chunks raisonnables (60s) pour éviter de saturer la RAM/VRAM
		// En Fast Mode, on envoie très peu de frames, donc on peut faire des chunks beaucoup plus longs (5 min)
		const CHUNK_DURATION = isHighFidelity ? 60000 : 300000;
		const fadeOutEndTimes: number[] = [];
		for (const clip of globalState.getSubtitleTrack.clips) {
			const { startTime, endTime } = clip as any;
			if (startTime == null || endTime == null) continue;
			if (endTime < exportStart || startTime > exportEnd) continue;
			if (!(clip instanceof SilenceClip)) {
				fadeOutEndTimes.push(endTime);
			}
		}
		for (const ctClip of globalState.getCustomClipTrack?.clips || []) {
			const category = (ctClip as any).category;
			if (!category) continue;
			const alwaysShow = (category.getStyle('always-show')?.value as number) || 0;
			const startTime = category.getStyle('time-appearance')?.value as number;
			const endTime = category.getStyle('time-disappearance')?.value as number;
			if (startTime == null || endTime == null) continue;
			if (endTime < exportStart || startTime > exportEnd) continue;
			if (!alwaysShow) {
				fadeOutEndTimes.push(endTime);
			}
		}
		const sortedFadeOutEnds = Array.from(new Set(fadeOutEndTimes))
			.filter((time) => time >= exportStart && time <= exportEnd)
			.sort((a, b) => a - b);

		const chunks: Array<{ start: number; end: number }> = [];
		let currentStart = exportStart;
		while (currentStart < exportEnd) {
			const idealChunkEnd = currentStart + (isHighFidelity ? 60000 : 300000);
			if (idealChunkEnd >= exportEnd) {
				chunks.push({ start: Math.round(currentStart), end: Math.round(exportEnd) });
				break;
			}
			const nextFadeOutEnd = sortedFadeOutEnds.find((time) => time >= idealChunkEnd);
			if (nextFadeOutEnd && nextFadeOutEnd <= exportEnd) {
				chunks.push({ start: Math.round(currentStart), end: Math.round(nextFadeOutEnd) });
				currentStart = nextFadeOutEnd;
			} else {
				const chunkEnd = Math.min(idealChunkEnd, exportEnd);
				chunks.push({ start: Math.round(currentStart), end: Math.round(chunkEnd) });
				currentStart = chunkEnd;
			}
		}
		return { chunks, fadeOutEndTimes: sortedFadeOutEnds };
	}

	async function wait(timing: number) {
		await new Promise((resolve) => setTimeout(resolve, 0));
		await new Promise((resolve) => setTimeout(resolve, 0));
		let subtitlesContainer = document.getElementById('subtitles-container');
		if (!subtitlesContainer) {
			await new Promise((resolve) => setTimeout(resolve, 50));
			return;
		}
		const startTime = Date.now();
		const timeout = 1500;
        let loops = 0;
		while (true) {
            loops++;
			const container = document.getElementById('subtitles-container');
			if (!container || container.style.opacity === '1') {
				break;
			}
			if (Date.now() - startTime > timeout) {
                console.warn(`[Wait] Timeout waiting for opacity=1 at ${timing}ms. Current: ${container?.style.opacity}`);
				break;
			}
			await new Promise((resolve) => setTimeout(resolve, 20));
		}
        // console.log(`[Wait] Ready at ${timing}ms after ${loops} loops`);
	}
</script>

{#if globalState.currentProject}
	<div class="absolute inset-0 w-screen h-screen">
		<VideoPreview bind:this={videoPreview} showControls={false} />
		<div class="hidden">
			<Timeline />
		</div>
	</div>
{/if}
