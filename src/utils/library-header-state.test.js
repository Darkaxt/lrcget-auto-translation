import { describe, expect, it } from 'vitest'

import {
  clearDisabledEmbedSelection,
  getDownloadButtonState,
  loadExportFormatPrefs,
  saveExportFormatPrefs,
} from './library-header-state.js'

describe('getDownloadButtonState', () => {
  it('shows the completed state when downloaded count reaches the aliased total count', () => {
    expect(
      getDownloadButtonState({
        isBuildingQueue: false,
        isDownloading: true,
        downloadedCount: 12,
        downloadTotalCount: 12,
      })
    ).toBe('downloaded')
  })

  it('shows the working state while downloads are still in progress', () => {
    expect(
      getDownloadButtonState({
        isBuildingQueue: false,
        isDownloading: true,
        downloadedCount: 11,
        downloadTotalCount: 12,
      })
    ).toBe('downloading')
  })
})

describe('export format preferences', () => {
  it('loads saved export format selections from storage', () => {
    const storage = new Map([
      [
        'lrcget:export-format-prefs',
        JSON.stringify({ plainText: true, syncedLrc: false, embedIntoTrack: true }),
      ],
    ])

    expect(loadExportFormatPrefs(storage)).toEqual({
      plainText: true,
      syncedLrc: false,
      embedIntoTrack: true,
    })
  })

  it('saves export format selections to storage', () => {
    const storage = new Map()

    saveExportFormatPrefs(storage, {
      plainText: false,
      syncedLrc: true,
      embedIntoTrack: false,
    })

    expect(JSON.parse(storage.get('lrcget:export-format-prefs'))).toEqual({
      plainText: false,
      syncedLrc: true,
      embedIntoTrack: false,
    })
  })

  it('clears a remembered embed selection when embed export is disabled', () => {
    expect(
      clearDisabledEmbedSelection({ plainText: true, syncedLrc: true, embedIntoTrack: true }, false)
    ).toEqual({ plainText: true, syncedLrc: true, embedIntoTrack: false })
  })
})
