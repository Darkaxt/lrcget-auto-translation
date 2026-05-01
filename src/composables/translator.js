import { computed, markRaw, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'

const delay = time => new Promise(resolve => setTimeout(resolve, time))
const SPEED_WINDOW_MS = 30000

const translateQueue = ref([])
const log = ref([])
const translatedCount = ref(0)
const skippedCount = ref(0)
const failureCount = ref(0)
const processedCount = ref(0)
const activeCount = ref(0)
const completionTimestamps = ref([])
const speedClock = ref(Date.now())
const isTranslating = ref(false)
const totalCount = ref(0)
const scheduledTrackIds = new Set()
let workersStarted = false
let speedClockTimer = null

const addLog = logObj => {
  log.value.unshift(markRaw(logObj))
  if (log.value.length > 100) {
    log.value.pop()
  }
}

const pruneCompletionTimestamps = now =>
  completionTimestamps.value.filter(timestamp => now - timestamp <= SPEED_WINDOW_MS)

const recordQueueCompletion = () => {
  const now = Date.now()
  speedClock.value = now
  completionTimestamps.value = [...pruneCompletionTimestamps(now), now]
}

const startSpeedClock = () => {
  if (speedClockTimer !== null) {
    return
  }

  speedClock.value = Date.now()
  speedClockTimer = window.setInterval(() => {
    const now = Date.now()
    speedClock.value = now
    completionTimestamps.value = pruneCompletionTimestamps(now)
  }, 1000)
}

const stopSpeedClock = () => {
  if (speedClockTimer === null) {
    return
  }

  window.clearInterval(speedClockTimer)
  speedClockTimer = null
}

const translateTrack = async track => {
  activeCount.value++

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
  } finally {
    activeCount.value = Math.max(0, activeCount.value - 1)

    if (isTranslating.value) {
      recordQueueCompletion()
      processedCount.value++
    }
  }
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
        recordQueueCompletion()
        processedCount.value++
      }
    } finally {
      scheduledTrackIds.delete(trackId)
    }

    await delay(1)
  }
}

const startTranslationWorkers = (workerCount = 3) => {
  if (workersStarted) {
    return
  }

  workersStarted = true
  for (let i = 0; i < workerCount; i++) {
    translateNext()
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

const translationSpeedPerSecond = computed(() => {
  if (!isTranslating.value) {
    return null
  }

  const now = speedClock.value
  const samples = completionTimestamps.value.filter(timestamp => now - timestamp <= SPEED_WINDOW_MS)
  if (samples.length === 0) {
    return null
  }

  const oldest = samples[0]
  const elapsedSeconds = Math.max(1, (now - oldest) / 1000)
  return samples.length / elapsedSeconds
})

const addToQueue = trackIds => {
  isTranslating.value = true
  startSpeedClock()
  let addedCount = 0

  for (let i = 0; i < trackIds.length; i++) {
    const trackId = trackIds[i]
    if (scheduledTrackIds.has(trackId)) {
      continue
    }
    scheduledTrackIds.add(trackId)
    translateQueue.value.push(trackId)
    addedCount++
  }

  totalCount.value += addedCount

  console.log(`Added ${addedCount} tracks to translation queue`)
}

const startOver = () => {
  translateQueue.value = []
  log.value = []
  translatedCount.value = 0
  skippedCount.value = 0
  failureCount.value = 0
  processedCount.value = 0
  activeCount.value = 0
  completionTimestamps.value = []
  speedClock.value = Date.now()
  totalCount.value = 0
  isTranslating.value = false
  scheduledTrackIds.clear()
  stopSpeedClock()
}

const stopTranslating = () => {
  startOver()
}

export function useTranslator() {
  return {
    isTranslating,
    translateQueue,
    translateProgress,
    translationSpeedPerSecond,
    translatedCount,
    skippedCount,
    failureCount,
    processedCount,
    activeCount,
    totalCount,
    log,
    addToQueue,
    startOver,
    stopTranslating,
    translateNext,
    startTranslationWorkers,
  }
}
