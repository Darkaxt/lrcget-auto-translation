import { computed, markRaw, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'

const delay = time => new Promise(resolve => setTimeout(resolve, time))

const translateQueue = ref([])
const log = ref([])
const translatedCount = ref(0)
const skippedCount = ref(0)
const failureCount = ref(0)
const processedCount = ref(0)
const isTranslating = ref(false)
const totalCount = ref(0)

const addLog = logObj => {
  log.value.unshift(markRaw(logObj))
  if (log.value.length > 100) {
    log.value.pop()
  }
}

const translateTrack = async track => {
  try {
    const result = await invoke('translate_track_lyrics', { trackId: track.id })

    if (!isTranslating.value) {
      return
    }

    if (result.status === 'skipped_same_language') {
      addLog({
        status: 'skipped',
        title: track.title,
        artistName: track.artist_name,
        message: result.error_message || 'Already in target language',
      })
      skippedCount.value++
    } else {
      addLog({
        status: 'translated',
        title: track.title,
        artistName: track.artist_name,
        message: 'Translated',
      })
      translatedCount.value++
    }
  } catch (error) {
    if (!isTranslating.value) {
      return
    }

    addLog({
      status: 'failure',
      title: track.title,
      artistName: track.artist_name,
      message: error,
    })
    failureCount.value++
  }

  processedCount.value++
}

const translateNext = async () => {
  while (true) {
    if (translateQueue.value.length === 0) {
      await delay(1000)
      continue
    }

    const trackId = translateQueue.value.shift()
    try {
      const track = await invoke('get_track', { trackId })
      await translateTrack(track)
    } catch (error) {
      if (isTranslating.value) {
        console.error('Failed to translate track:', error)
        failureCount.value++
        processedCount.value++
      }
    }

    await delay(1)
  }
}

const translateProgress = computed(() => {
  if (!isTranslating.value || totalCount.value === 0) {
    return 0.0
  }

  if (processedCount.value >= totalCount.value) {
    return 1.0
  }

  return processedCount.value / totalCount.value
})

const addToQueue = trackIds => {
  isTranslating.value = true

  for (let i = 0; i < trackIds.length; i++) {
    translateQueue.value.push(trackIds[i])
  }

  totalCount.value += trackIds.length

  console.log(`Added ${trackIds.length} tracks to translation queue`)
}

const startOver = () => {
  translateQueue.value = []
  log.value = []
  translatedCount.value = 0
  skippedCount.value = 0
  failureCount.value = 0
  processedCount.value = 0
  totalCount.value = 0
  isTranslating.value = false
}

const stopTranslating = () => {
  startOver()
}

export function useTranslator() {
  return {
    isTranslating,
    translateQueue,
    translateProgress,
    translatedCount,
    skippedCount,
    failureCount,
    processedCount,
    totalCount,
    log,
    addToQueue,
    startOver,
    stopTranslating,
    translateNext,
  }
}
