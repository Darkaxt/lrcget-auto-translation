import { ref, onMounted, onUnmounted } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

const playingTrack = ref(null)
const status = ref('stopped')
const duration = ref(null)
const progress = ref(null)
const volume = ref(1.0)

listen('player-state', async event => {
  duration.value = event.payload.duration
  progress.value = event.payload.progress
  status.value = event.payload.status
  volume.value = event.payload.volume
})

listen('reload-track-id', async event => {
  const payload = event.payload
  if (playingTrack.value && playingTrack.value.id === payload) {
    playingTrack.value = await invoke('get_track', { trackId: playingTrack.value.id })
  }
})

export function usePlayer() {
  const playTrack = async track => {
    const previousTrack = playingTrack.value

    try {
      // Determine if this is a database track or a file-based track
      if (track.id !== undefined && track.id !== null) {
        // Database track - use track_id
        await invoke('play_track', {
          trackId: track.id,
          filePath: null,
          title: track.title,
          albumName: track.album_name,
          artistName: track.artist_name,
          albumArtistName: track.album_artist_name,
          duration: track.duration,
        })
      } else if (track.file_path) {
        // File-based track (from file picker) - use file_path with metadata
        await invoke('play_track', {
          trackId: null,
          filePath: track.file_path,
          title: track.title,
          albumName: track.album_name,
          artistName: track.artist_name,
          albumArtistName: track.album_artist_name,
          duration: track.duration,
        })
      } else {
        throw new Error('No playable track path or library ID was provided')
      }

      playingTrack.value = track
    } catch (error) {
      playingTrack.value = previousTrack
      throw error
    }
  }

  const pause = () => {
    if (!playingTrack.value) {
      return
    }

    return invoke('pause_track')
  }

  const resume = () => {
    if (!playingTrack.value) {
      return
    }

    return invoke('resume_track')
  }

  const seek = async position => {
    if (!playingTrack.value) {
      return
    }

    if (status.value === 'stopped') {
      await playTrack(playingTrack.value)
    }

    return invoke('seek_track', { position })
  }

  const stop = () => {
    if (!playingTrack.value) {
      return
    }

    return invoke('stop_track')
  }

  const setVolume = volume => {
    return invoke('set_volume', { volume })
  }

  return {
    playingTrack,
    status,
    duration,
    progress,
    volume,
    playTrack,
    pause,
    resume,
    stop,
    seek,
    setVolume,
  }
}
