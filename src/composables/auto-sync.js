import { computed, markRaw, ref } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

const delay = time => new Promise(resolve => setTimeout(resolve, time))
const SPEED_WINDOW_MS = 30000

const syncQueue = ref([])
const log = ref([])
const engineLog = ref([])
const syncedCount = ref(0)
const reviewCount = ref(0)
const failureCount = ref(0)
const processedCount = ref(0)
const activeCount = ref(0)
const totalCount = ref(0)
const completionTimestamps = ref([])
const speedClock = ref(Date.now())
const isAutoSyncing = ref(false)
const scheduledTrackIds = new Set()
let workersStarted = false
let engineEventListenersStarted = false
let speedClockTimer = null

const addLog = logObj => {
  log.value.unshift(markRaw({ ...logObj, createdAt: Date.now() }))
  if (log.value.length > 100) {
    log.value.pop()
  }
}

const resetRunState = () => {
  syncQueue.value = []
  log.value = []
  engineLog.value = []
  syncedCount.value = 0
  reviewCount.value = 0
  failureCount.value = 0
  processedCount.value = 0
  activeCount.value = 0
  totalCount.value = 0
  completionTimestamps.value = []
  speedClock.value = Date.now()
  scheduledTrackIds.clear()
}

const trackArtistName = track => track?.artist_name || track?.artistName || ''

const addResultLog = (track, result) => {
  if (result.status === 'succeeded') {
    syncedCount.value++
    addLog({
      status: 'synced',
      title: track.title,
      artistName: trackArtistName(track),
      message: `Synced (${Math.round((result.confidence || 0) * 100)}%)`,
    })
  } else if (result.status === 'needs_review') {
    reviewCount.value++
    addLog({
      status: 'review',
      title: track.title,
      artistName: trackArtistName(track),
      message: `Needs review (${Math.round((result.confidence || 0) * 100)}%)`,
    })
  } else {
    failureCount.value++
    addLog({
      status: 'failure',
      title: track.title,
      artistName: trackArtistName(track),
      message: result.errorMessage || result.error_message || 'Sync failed',
    })
  }
}

const addEngineLog = (status, event) => {
  engineLog.value.push(
    markRaw({
      status,
      trackId: event.trackId,
      title: event.trackTitle,
      artistName: event.artistName,
      phase: event.phase,
      message: event.message,
      stream: event.stream,
      elapsedMs: event.elapsedMs,
      exitCode: event.exitCode,
      createdAt: Date.now(),
    })
  )
  if (engineLog.value.length > 500) {
    engineLog.value.shift()
  }
}

const startAutoSyncEventListeners = async () => {
  if (engineEventListenersStarted) {
    return
  }

  engineEventListenersStarted = true
  await listen('auto-sync-engine-started', event => addEngineLog('started', event.payload))
  await listen('auto-sync-engine-log', event => addEngineLog('log', event.payload))
  await listen('auto-sync-engine-finished', event => addEngineLog('finished', event.payload))
  await listen('auto-sync-engine-failed', event => addEngineLog('failed', event.payload))
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

const syncTrack = async track => {
  activeCount.value++

  try {
    const result = await invoke('auto_sync_track_lyrics', { trackId: track.id })

    if (!isAutoSyncing.value) {
      return
    }

    addResultLog(track, result)
  } catch (error) {
    if (!isAutoSyncing.value) {
      return
    }
    failureCount.value++
    addLog({
      status: 'failure',
      title: track.title,
      artistName: trackArtistName(track),
      message: error,
    })
  } finally {
    activeCount.value = Math.max(0, activeCount.value - 1)
    if (isAutoSyncing.value) {
      recordQueueCompletion()
      processedCount.value++
    }
  }
}

const syncNext = async () => {
  while (true) {
    if (syncQueue.value.length === 0) {
      await delay(1000)
      continue
    }

    const trackId = syncQueue.value.shift()
    try {
      const track = await invoke('get_track', { trackId })
      await syncTrack(track)
    } catch (error) {
      if (isAutoSyncing.value) {
        console.error('Failed to auto-sync track:', error)
        failureCount.value++
        recordQueueCompletion()
        processedCount.value++
      }
    } finally {
      scheduledTrackIds.delete(trackId)
    }
  }
}

const startAutoSyncWorkers = (workerCount = 1) => {
  if (workersStarted) {
    return
  }
  workersStarted = true
  for (let i = 0; i < workerCount; i++) {
    syncNext()
  }
}

const addToQueue = trackIds => {
  isAutoSyncing.value = true
  startSpeedClock()
  let addedCount = 0

  for (const trackId of trackIds) {
    if (scheduledTrackIds.has(trackId)) {
      continue
    }
    scheduledTrackIds.add(trackId)
    syncQueue.value.push(trackId)
    addedCount++
  }

  totalCount.value += addedCount
}

const startOver = () => {
  resetRunState()
  isAutoSyncing.value = false
  stopSpeedClock()
}

const startManualRun = track => {
  if (!isAutoSyncing.value) {
    resetRunState()
  }

  isAutoSyncing.value = true
  startSpeedClock()
  totalCount.value = 1
  activeCount.value = 1
  if (track?.id !== undefined && track?.id !== null) {
    scheduledTrackIds.add(track.id)
  }
  addLog({
    status: 'started',
    title: track.title,
    artistName: trackArtistName(track),
    message: 'Started auto-sync',
  })
}

const finishManualRun = (track, result) => {
  activeCount.value = Math.max(0, activeCount.value - 1)
  addResultLog(track, result)
  recordQueueCompletion()
  processedCount.value = Math.min(totalCount.value || 1, processedCount.value + 1)
  if (track?.id !== undefined && track?.id !== null) {
    scheduledTrackIds.delete(track.id)
  }
}

const failManualRun = (track, error) => {
  activeCount.value = Math.max(0, activeCount.value - 1)
  failureCount.value++
  addLog({
    status: 'failure',
    title: track.title,
    artistName: trackArtistName(track),
    message: error?.toString?.() || error || 'Sync failed',
  })
  recordQueueCompletion()
  processedCount.value = Math.min(totalCount.value || 1, processedCount.value + 1)
  if (track?.id !== undefined && track?.id !== null) {
    scheduledTrackIds.delete(track.id)
  }
}

const autoSyncProgress = computed(() => {
  if (!isAutoSyncing.value || totalCount.value === 0) {
    return 0
  }
  return Math.min(1, processedCount.value / totalCount.value)
})

const autoSyncSpeedPerSecond = computed(() => {
  if (!isAutoSyncing.value) {
    return null
  }
  const now = speedClock.value
  const samples = completionTimestamps.value.filter(timestamp => now - timestamp <= SPEED_WINDOW_MS)
  if (samples.length === 0) {
    return null
  }
  const elapsedSeconds = Math.max(1, (now - samples[0]) / 1000)
  return samples.length / elapsedSeconds
})

export function useAutoSync() {
  return {
    isAutoSyncing,
    syncQueue,
    autoSyncProgress,
    autoSyncSpeedPerSecond,
    syncedCount,
    reviewCount,
    failureCount,
    processedCount,
    activeCount,
    totalCount,
    log,
    engineLog,
    addToQueue,
    startOver,
    stopAutoSyncing: startOver,
    startAutoSyncWorkers,
    startAutoSyncEventListeners,
    startManualRun,
    finishManualRun,
    failManualRun,
  }
}
