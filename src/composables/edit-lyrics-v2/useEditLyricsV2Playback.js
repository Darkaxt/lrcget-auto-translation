export function useEditLyricsV2Playback({
  audioSource,
  syncedLines,
  progress,
  playingTrack,
  status,
  playTrack,
  resume,
  pause,
  seek,
  onPlaybackError,
}) {
  let linePreviewTimer = null

  const clearLinePreview = () => {
    if (linePreviewTimer === null) {
      return
    }

    clearTimeout(linePreviewTimer)
    linePreviewTimer = null
  }

  // Helper to check if the playing track matches the audio source
  const isPlayingCorrectTrack = () => {
    if (!playingTrack.value || !audioSource.value) {
      return false
    }
    return audioSource.value.type === 'library'
      ? playingTrack.value.id === audioSource.value.id
      : playingTrack.value.file_path === audioSource.value.file_path
  }

  const playLine = async lineIndex => {
    try {
      if (!audioSource.value) {
        return
      }

      clearLinePreview()

      const line = syncedLines.value[lineIndex]
      const lineStartMs = line?.start_ms
      const seekTo = Number.isFinite(lineStartMs) ? lineStartMs / 1000 : progress.value

      if (!isPlayingCorrectTrack()) {
        await playTrack(audioSource.value)
      } else if (status.value === 'paused') {
        await resume()
      }

      await seek(seekTo)
      scheduleLinePreviewStop(lineIndex, lineStartMs)
    } catch (error) {
      onPlaybackError?.(error)
    }
  }

  const resumeOrPlay = async () => {
    try {
      if (status.value === 'paused' && isPlayingCorrectTrack()) {
        await resume()
        return
      }

      if (audioSource.value) {
        clearLinePreview()
        await playTrack(audioSource.value)
      }
    } catch (error) {
      onPlaybackError?.(error)
    }
  }

  const pauseEditorPlayback = async () => {
    try {
      clearLinePreview()
      await pause?.()
    } catch (error) {
      onPlaybackError?.(error)
    }
  }

  const seekEditorPlayback = async position => {
    try {
      clearLinePreview()
      await seek(position)
    } catch (error) {
      onPlaybackError?.(error)
    }
  }

  const scheduleLinePreviewStop = (lineIndex, lineStartMs) => {
    const lineEndMs = getLineEndMs(lineIndex)
    if (!Number.isFinite(lineStartMs) || !Number.isFinite(lineEndMs) || lineEndMs <= lineStartMs) {
      return
    }

    linePreviewTimer = setTimeout(async () => {
      linePreviewTimer = null
      try {
        if (isPlayingCorrectTrack()) {
          await pause?.()
        }
      } catch (error) {
        onPlaybackError?.(error)
      }
    }, lineEndMs - lineStartMs)
  }

  const getLineEndMs = lineIndex => {
    const line = syncedLines.value[lineIndex]
    const lineStartMs = line?.start_ms
    if (!Number.isFinite(lineStartMs)) {
      return null
    }

    if (Number.isFinite(line.end_ms)) {
      return line.end_ms
    }

    const nextLineStartMs = syncedLines.value[lineIndex + 1]?.start_ms
    if (Number.isFinite(nextLineStartMs) && nextLineStartMs > lineStartMs) {
      return nextLineStartMs
    }

    return null
  }

  return {
    clearLinePreview,
    pauseEditorPlayback,
    playLine,
    resumeOrPlay,
    seekEditorPlayback,
  }
}
