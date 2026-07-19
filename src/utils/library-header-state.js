export const EXPORT_FORMAT_PREFS_STORAGE_KEY = 'lrcget:export-format-prefs'

const DEFAULT_EXPORT_FORMAT_PREFS = {
  plainText: false,
  syncedLrc: false,
  embedIntoTrack: false,
}

const storageGet = (storage, key) =>
  typeof storage.getItem === 'function' ? storage.getItem(key) : storage.get(key)

const storageSet = (storage, key, value) => {
  if (typeof storage.setItem === 'function') {
    storage.setItem(key, value)
    return
  }

  storage.set(key, value)
}

export const getDownloadButtonState = ({
  isBuildingQueue,
  isDownloading,
  downloadedCount,
  downloadTotalCount,
}) => {
  if (isBuildingQueue) {
    return 'preparing'
  }

  if (isDownloading && downloadedCount !== downloadTotalCount) {
    return 'downloading'
  }

  if (isDownloading) {
    return 'downloaded'
  }

  return 'idle'
}

export const normalizeExportFormatPrefs = value => ({
  plainText: value?.plainText === true,
  syncedLrc: value?.syncedLrc === true,
  embedIntoTrack: value?.embedIntoTrack === true,
})

export const loadExportFormatPrefs = storage => {
  if (!storage) {
    return { ...DEFAULT_EXPORT_FORMAT_PREFS }
  }

  try {
    const raw = storageGet(storage, EXPORT_FORMAT_PREFS_STORAGE_KEY)
    return normalizeExportFormatPrefs(raw ? JSON.parse(raw) : null)
  } catch (error) {
    console.error('Failed to load export format preferences', error)
    return { ...DEFAULT_EXPORT_FORMAT_PREFS }
  }
}

export const saveExportFormatPrefs = (storage, prefs) => {
  if (!storage) {
    return
  }

  try {
    storageSet(
      storage,
      EXPORT_FORMAT_PREFS_STORAGE_KEY,
      JSON.stringify(normalizeExportFormatPrefs(prefs))
    )
  } catch (error) {
    console.error('Failed to save export format preferences', error)
  }
}

export const clearDisabledEmbedSelection = (prefs, canEmbed) => ({
  ...normalizeExportFormatPrefs(prefs),
  embedIntoTrack: canEmbed ? prefs?.embedIntoTrack === true : false,
})
