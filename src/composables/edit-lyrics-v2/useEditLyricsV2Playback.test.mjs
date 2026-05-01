import assert from 'node:assert/strict'
import { test } from 'node:test'
import { ref } from 'vue'

import { useEditLyricsV2Playback } from './useEditLyricsV2Playback.js'

const wait = ms => new Promise(resolve => setTimeout(resolve, ms))

test('playLine plays only the selected line duration', async () => {
  const calls = []
  const audioSource = ref({ type: 'library', id: 1, title: 'Track' })
  const syncedLines = ref([{ start_ms: 10, end_ms: 35, text: 'line one' }])
  const playingTrack = ref(null)
  const status = ref('stopped')

  const { playLine } = useEditLyricsV2Playback({
    audioSource,
    syncedLines,
    progress: ref(0),
    playingTrack,
    status,
    playTrack: async track => {
      calls.push(['playTrack', track.id])
      playingTrack.value = track
      status.value = 'playing'
    },
    resume: async () => {
      calls.push(['resume'])
      status.value = 'playing'
    },
    pause: async () => {
      calls.push(['pause'])
      status.value = 'paused'
    },
    seek: async position => {
      calls.push(['seek', position])
    },
  })

  await playLine(0)
  await wait(60)

  assert.deepEqual(calls, [
    ['playTrack', 1],
    ['seek', 0.01],
    ['pause'],
  ])
})

test('editor seek cancels pending line preview pause', async () => {
  const calls = []
  const audioSource = ref({ type: 'library', id: 1, title: 'Track' })
  const syncedLines = ref([{ start_ms: 10, end_ms: 70, text: 'line one' }])
  const playingTrack = ref(null)
  const status = ref('stopped')

  const { playLine, seekEditorPlayback } = useEditLyricsV2Playback({
    audioSource,
    syncedLines,
    progress: ref(0),
    playingTrack,
    status,
    playTrack: async track => {
      calls.push(['playTrack', track.id])
      playingTrack.value = track
      status.value = 'playing'
    },
    resume: async () => {
      calls.push(['resume'])
      status.value = 'playing'
    },
    pause: async () => {
      calls.push(['pause'])
      status.value = 'paused'
    },
    seek: async position => {
      calls.push(['seek', position])
    },
  })

  await playLine(0)
  await wait(10)
  await seekEditorPlayback(0.2)
  await wait(80)

  assert.deepEqual(calls, [
    ['playTrack', 1],
    ['seek', 0.01],
    ['seek', 0.2],
  ])
})
