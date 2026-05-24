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

  const isPlayingCorrectTrack = () => {
    if (!playingTrack.value || !audioSource.value) {
      return false
    }
    return audioSource.value.type === 'library'
      ? playingTrack.value.id === audioSource.value.id
      : playingTrack.value.file_path === audioSource.value.file_path
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

  const scheduleLinePreviewStop = (lineIndex, playbackStartMs) => {
    const lineEndMs = getLineEndMs(lineIndex)
    if (
      !Number.isFinite(playbackStartMs) ||
      !Number.isFinite(lineEndMs) ||
      lineEndMs <= playbackStartMs
    ) {
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
    }, lineEndMs - playbackStartMs)
  }

  const playLineAtOffset = async (lineIndex, offsetMs = 0) => {
    try {
      if (!audioSource.value) {
        return
      }

      clearLinePreview()

      const lineStartMs = syncedLines.value[lineIndex]?.start_ms
      const baseStartMs = Number.isFinite(lineStartMs) ? lineStartMs : progress.value * 1000
      const playbackStartMs = Math.max(0, baseStartMs + offsetMs)

      if (!isPlayingCorrectTrack()) {
        await playTrack(audioSource.value)
      } else if (status.value === 'paused') {
        await resume()
      }

      await seek(playbackStartMs / 1000)
      scheduleLinePreviewStop(lineIndex, playbackStartMs)
    } catch (error) {
      onPlaybackError?.(error)
    }
  }

  const playLine = async lineIndex => {
    return playLineAtOffset(lineIndex, 0)
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

  return {
    clearLinePreview,
    pauseEditorPlayback,
    playLine,
    playLineAtOffset,
    resumeOrPlay,
    seekEditorPlayback,
  }
}
