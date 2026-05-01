<template>
  <BaseModal
    :click-to-close="false"
    :esc-to-close="false"
    content-class="w-full h-[95vh] max-w-screen-lg"
    :title="modalTitle"
    @close="handleClose"
  >
    <template #titleLeft>
      <EditLyricsV2HeaderActions
        :is-dirty="isDirty"
        :is-exporting="isExporting"
        @save="saveLyrics"
        @save-and-publish="saveAndPublish"
        @export="exportLyrics"
        @debug="openDebugModal"
      />
    </template>

    <template #titleRight>
      <div class="inline-flex items-center gap-2">
        <button
          v-if="canAutoSync"
          class="button text-sm h-8 px-3 rounded-full"
          :class="isAutoSyncing ? 'button-disabled' : 'button-normal'"
          :disabled="isAutoSyncing"
          @click="runAutoSync"
        >
          {{ isAutoSyncing ? 'Syncing' : 'Auto-sync' }}
        </button>
        <div class="inline-flex gap-0.5">
        <button
          class="button text-sm h-8 w-24 rounded-l-full rounded-r-none"
          :class="
            isInstrumental
              ? 'button-disabled'
              : activeTab === 'plain'
                ? 'button-primary'
                : 'button-normal'
          "
          :disabled="isInstrumental"
          @click="activeTab = 'plain'"
        >
          Plain
        </button>
        <button
          class="button text-sm h-8 w-24 rounded-r-full rounded-l-none"
          :class="
            isInstrumental
              ? 'button-disabled'
              : activeTab === 'synced'
                ? 'button-primary'
                : 'button-normal'
          "
          :disabled="isInstrumental"
          @click="activeTab = 'synced'"
        >
          Synced
        </button>
        </div>
      </div>
    </template>

    <div class="grow overflow-hidden flex flex-col gap-2 h-full">
      <div class="toolbar bg-neutral-100 dark:bg-neutral-800 rounded-lg border border-neutral-200 dark:border-neutral-700">
        <EditLyricsV2PlayerBar
          :status="editorStatus"
          :duration="editorDuration"
          :progress="editorProgress"
          @play-toggle="resumeOrPlay"
          @pause="pauseEditorPlayback"
          @seek="seekEditorPlayback"
        />
      </div>

      <div
        v-if="autoSyncPreview"
        class="rounded-lg border border-neutral-200 dark:border-neutral-700 bg-neutral-50 dark:bg-neutral-900 p-3"
      >
        <div class="flex items-center justify-between gap-3 mb-2">
          <div class="text-sm font-bold text-neutral-900 dark:text-neutral-100">
            Auto-sync preview · {{ autoSyncPreviewConfidence }}
          </div>
          <div class="flex gap-2">
            <button class="button button-primary h-8 px-3 rounded" @click="applyAutoSyncPreview">
              Apply
            </button>
            <button class="button button-normal h-8 px-3 rounded" @click="discardAutoSyncPreview">
              Discard
            </button>
          </div>
        </div>
        <div class="grid grid-cols-2 gap-3 text-xs max-h-40 overflow-hidden">
          <pre class="overflow-auto whitespace-pre-wrap text-neutral-700 dark:text-neutral-300">{{ plainLyrics }}</pre>
          <pre class="overflow-auto whitespace-pre-wrap text-neutral-700 dark:text-neutral-300">{{ autoSyncPreview.generatedLrc }}</pre>
        </div>
      </div>

      <!-- Instrumental State -->
      <div v-if="isInstrumental" class="absolute bottom-16 left-1/2 -translate-x-1/2 px-3 z-10">
        <div
          class="w-full max-w-lg rounded-lg border border-neutral-200 dark:border-neutral-700 bg-white dark:bg-neutral-900 p-5 shadow-lg"
        >
          <h3 class="text-base font-semibold text-neutral-900 dark:text-neutral-200">Track is marked as instrumental</h3>
          <div class="mt-4 flex flex-wrap gap-2">
            <button
              class="button button-normal px-2 py-1 text-xs rounded-full"
              @click="setInstrumental(false)"
            >
              Unmark as instrumental
            </button>
          </div>
        </div>
      </div>

      <PlainLyricsCodeEditor
        v-else-if="activeTab === 'plain'"
        :model-value="plainLyrics"
        :font-size="codemirrorStyle.fontSize"
        :synced-lines="syncedLines"
        @update:model-value="updatePlainLyrics"
        @change-font-size="changeCodemirrorFontSizeBy"
        @reset-font-size="resetCodemirrorFontSize"
        @mark-as-instrumental="setInstrumental(true)"
      />

      <SyncedLyricsEditor
        v-else
        :model-value="syncedLines"
        :can-import-from-plain="hasPlainLyrics"
        :selected-line-index="selectedSyncedLineIndex"
        :progress-ms="progressMs"
        @update:model-value="updateSyncedLines"
        @update:selected-line-index="selectSyncedLine"
        @editing-state-change="setSyncedLineEditingState"
        @play-line="playLine"
        @sync-line="syncLineToCurrentProgress"
        @rewind-line="rewindLineBy100"
        @forward-line="forwardLineBy100"
        @sync-end="syncEndToCurrentProgress"
        @rewind-end="rewindEndBy100"
        @forward-end="forwardEndBy100"
        @delete-line="deleteSyncedLine"
        @add-line-at="addSyncedLineAt"
        @import-lines-from-plain="importSyncedLinesFromPlain"
        @import-lrc-file="handleImportLrcFile"
        @paste-lrc="handlePasteLrc"
        @update:words="updateLineWords"
        @word-timing-edited="handleWordTimingEdited"
        @update-line-text="handleUpdateLineText"
        @mark-as-instrumental="setInstrumental(true)"
      />
    </div>
  </BaseModal>
</template>

<script setup>
import { computed, onMounted, onUnmounted, ref, toRef, watch } from 'vue'
import { useToast } from 'vue-toastification'
import { useModal } from 'vue-final-modal'
import BaseModal from '@/components/common/BaseModal.vue'
import ConfirmModal from '@/components/common/ConfirmModal.vue'
import EditLyricsV2DebugModal from '@/components/library/edit-lyrics-v2/EditLyricsV2DebugModal.vue'
import EditLyricsV2HeaderActions from '@/components/library/edit-lyrics-v2/EditLyricsV2HeaderActions.vue'
import EditLyricsV2PlayerBar from '@/components/library/edit-lyrics-v2/EditLyricsV2PlayerBar.vue'
import AutoSyncViewer from '@/components/library/AutoSyncViewer.vue'
import PlainLyricsCodeEditor from '@/components/library/edit-lyrics-v2/PlainLyricsCodeEditor.vue'
import SyncedLyricsEditor from '@/components/library/edit-lyrics-v2/SyncedLyricsEditor.vue'
import { useEditLyricsV2Document } from '@/composables/edit-lyrics-v2/useEditLyricsV2Document.js'
import { useEditLyricsV2Hotkeys } from '@/composables/edit-lyrics-v2/useEditLyricsV2Hotkeys.js'
import { useEditLyricsV2Publish } from '@/composables/edit-lyrics-v2/useEditLyricsV2Publish.js'
import { useEditLyricsV2Playback } from '@/composables/edit-lyrics-v2/useEditLyricsV2Playback.js'
import { useEditLyricsV2Export } from '@/composables/edit-lyrics-v2/useEditLyricsV2Export.js'
import { useEditLyricsV2SyncedHotkeys } from '@/composables/edit-lyrics-v2/useEditLyricsV2SyncedHotkeys.js'
import { useGlobalState } from '@/composables/global-state.js'
import { usePlayer } from '@/composables/player.js'
import { useAutoSync } from '@/composables/auto-sync.js'
import { open } from '@tauri-apps/plugin-dialog'
import { readText } from '@tauri-apps/plugin-clipboard-manager'
import { invoke } from '@tauri-apps/api/core'
import { parseLrcLines } from '@/utils/lyricsfile.js'

const props = defineProps({
  // Audio source for playback (library track or file-based track)
  // Format: { type: 'library'|'file', id?, file_path?, duration?, title?, artist_name?, album_name?, ... }
  audioSource: {
    type: Object,
    required: true,
  },
  // Lyricsfile object for editing operations (save, debug, publish)
  // Format: { id?, content, metadata?: { title, artist, album, duration_ms } }
  // For library tracks, id is null and content comes from track.lyricsfile
  // For standalone lyricsfiles, id is the lyricsfiles table record ID
  lyricsfile: {
    type: Object,
    default: null,
  },
  // Track ID for save operations. Set for library tracks, null for temporary associations
  // This is separate from audioSource to handle the case where a library track is temporarily
  // associated with a standalone lyricsfile (e.g., LRCLIB Browser flow)
  trackId: {
    type: Number,
    default: null,
  },
})

const emit = defineEmits(['close'])

const { disableHotkey, enableHotkey } = useGlobalState()
const { status, duration, progress, playingTrack, playTrack, pause, resume, seek } = usePlayer()
const {
  startManualRun: startManualAutoSyncRun,
  finishManualRun: finishManualAutoSyncRun,
  failManualRun: failManualAutoSyncRun,
} = useAutoSync()
const toast = useToast()

// Convert props to refs for composables
const audioSourceRef = toRef(props, 'audioSource')
const lyricsfileRef = toRef(props, 'lyricsfile')
const trackIdRef = toRef(props, 'trackId')

const activeTab = ref('plain')
const isAutoSyncing = ref(false)
const autoSyncPreview = ref(null)
const {
  plainLyrics,
  syncedLines,
  lyricsfileDocument,
  isDirty,
  selectedSyncedLineIndex,
  isSyncedLineEditing,
  hasPlainLyrics,
  selectedLineExists,
  isInstrumental,
  serializedLyricsfile,
  initializeLyrics,
  replaceSyncedLines,
  updatePlainLyrics,
  updateSyncedLines,
  selectSyncedLine,
  setSyncedLineEditingState,
  addSyncedLineAt,
  deleteSyncedLine,
  importSyncedLinesFromPlain,
  syncLineToCurrentProgress,
  rewindLineBy100: rewindLineTimestampBy100,
  forwardLineBy100: forwardLineTimestampBy100,
  syncEndToCurrentProgress,
  rewindEndBy100,
  forwardEndBy100,
  saveLyrics,
  ensureSelectedSyncedLine,
  updateLineText,
  setInstrumental,
} = useEditLyricsV2Document({
  audioSource: audioSourceRef,
  lyricsfile: lyricsfileRef,
  trackId: trackIdRef,
  progress,
  toast,
})

const isPlayingEditorTrack = computed(() => {
  if (!playingTrack.value || !audioSourceRef.value) {
    return false
  }

  return audioSourceRef.value.type === 'library'
    ? playingTrack.value.id === audioSourceRef.value.id
    : playingTrack.value.file_path === audioSourceRef.value.file_path
})

const editorStatus = computed(() => (isPlayingEditorTrack.value ? status.value : 'stopped'))
const editorDuration = computed(() =>
  isPlayingEditorTrack.value
    ? duration.value || audioSourceRef.value?.duration || 0
    : audioSourceRef.value?.duration || 0
)
const editorProgress = computed(() => (isPlayingEditorTrack.value ? progress.value || 0 : 0))
const progressMs = computed(() => Math.max(0, Math.round(editorProgress.value * 1000)))

const codemirrorStyle = ref({
  fontSize: 1.0,
})

const canAutoSync = computed(() => {
  return (
    props.trackId !== null &&
    !isInstrumental.value &&
    hasPlainLyrics.value &&
    syncedLines.value.length === 0
  )
})

const autoSyncPreviewConfidence = computed(() => {
  const confidence = autoSyncPreview.value?.confidence
  if (!Number.isFinite(confidence)) {
    return 'confidence unknown'
  }
  return `${Math.round(confidence * 100)}% confidence`
})

const autoSyncViewerTrack = computed(() => ({
  id: props.trackId,
  title: audioSourceRef.value?.title || lyricsfileRef.value?.metadata?.title || 'Unknown Title',
  artist_name:
    audioSourceRef.value?.artist_name || lyricsfileRef.value?.metadata?.artist || 'Unknown Artist',
}))

const { saveAndPublish } = useEditLyricsV2Publish({
  audioSource: audioSourceRef,
  lyricsfileDocument: lyricsfileDocument,
  serializedLyricsfile,
  saveLyrics,
})

const { exportLyrics, isExporting } = useEditLyricsV2Export({
  audioSource: audioSourceRef,
  saveLyrics,
  serializedLyricsfile,
  toast,
})

const { clearLinePreview, pauseEditorPlayback, playLine, resumeOrPlay, seekEditorPlayback } =
  useEditLyricsV2Playback({
    audioSource: audioSourceRef,
    syncedLines,
    progress: editorProgress,
    playingTrack,
    status: editorStatus,
    playTrack,
    resume,
    pause,
    seek,
    onPlaybackError: error => {
      console.error('Editor playback failed:', error)
      toast.error(`Playback failed: ${error}`)
    },
  })

const rewindLineBy100 = lineIndex => {
  rewindLineTimestampBy100(lineIndex)
  void playLine(lineIndex)
}

const forwardLineBy100 = lineIndex => {
  forwardLineTimestampBy100(lineIndex)
  void playLine(lineIndex)
}

const updateLineWords = ({ lineIndex, words, lineStartMs }) => {
  if (!Number.isInteger(lineIndex) || lineIndex < 0 || lineIndex >= syncedLines.value.length) {
    return
  }

  const nextLineStartMs = Number.isFinite(lineStartMs) ? Math.max(0, Math.round(lineStartMs)) : null

  const newLines = syncedLines.value.map((line, index) => {
    if (index !== lineIndex) {
      return line
    }

    return {
      ...line,
      ...(nextLineStartMs === null ? {} : { start_ms: nextLineStartMs }),
      words,
    }
  })

  updateSyncedLines(newLines)
}

const handleUpdateLineText = (lineIndex, newText) => {
  updateLineText(lineIndex, newText)
}

const handleImportLrcFile = async () => {
  try {
    const filePath = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: 'LRC Files', extensions: ['lrc'] },
        { name: 'All Files', extensions: ['*'] },
      ],
    })

    if (!filePath) {
      return
    }

    const content = await invoke('read_text_file', { filePath })
    const parsedLines = parseLrcLines(content)

    if (parsedLines.length === 0) {
      toast.error('No valid synced lines found in the selected file')
      return
    }

    updateSyncedLines(parsedLines)
    toast.success(`Imported ${parsedLines.length} synced lines`)
  } catch (error) {
    console.error(error)
    toast.error(error?.toString?.() || 'Failed to import LRC file')
  }
}

const handlePasteLrc = async () => {
  try {
    const text = await readText()
    if (!text || !text.trim()) {
      toast.error('Clipboard is empty')
      return
    }

    const parsedLines = parseLrcLines(text)

    if (parsedLines.length === 0) {
      toast.error('No valid synced lines found in clipboard')
      return
    }

    updateSyncedLines(parsedLines)
    toast.success(`Imported ${parsedLines.length} synced lines`)
  } catch (error) {
    console.error(error)
    toast.error(error?.toString?.() || 'Failed to paste LRC from clipboard')
  }
}

const handleWordTimingEdited = async ({ startMs }) => {
  // Auto-replay from the beginning of the edited line for instant verification
  const seekTo = Number.isFinite(startMs) ? startMs / 1000 : editorProgress.value

  try {
    if (!isPlayingEditorTrack.value) {
      await playTrack(audioSourceRef.value)
    } else if (editorStatus.value === 'paused') {
      await resume()
    }

    await seek(seekTo)
  } catch (error) {
    console.error('Editor playback failed:', error)
    toast.error(`Playback failed: ${error}`)
  }
}

const { open: openAutoSyncViewer, close: closeAutoSyncViewer } = useModal({
  component: AutoSyncViewer,
  attrs: {
    onClose() {
      closeAutoSyncViewer()
    },
  },
})

const restoreAutoSyncReviewDraft = async () => {
  if (props.trackId === null || autoSyncPreview.value || syncedLines.value.length > 0) {
    return
  }

  try {
    const results = await invoke('list_track_sync_results', { trackId: props.trackId })
    const sourceLyricsfile = lyricsfileRef.value?.content ?? ''
    const review = results.find(
      result =>
        result.status === 'needs_review' &&
        result.generatedLrc &&
        (!sourceLyricsfile || result.sourceLyricsfile === sourceLyricsfile)
    )

    if (!review) {
      return
    }

    const parsedLines = parseLrcLines(review.generatedLrc || '')
    if (parsedLines.length === 0) {
      return
    }

    replaceSyncedLines(parsedLines, { markDirty: false })
    autoSyncPreview.value = {
      id: review.id,
      generatedLrc: review.generatedLrc || '',
      confidence: review.confidence,
    }
    activeTab.value = 'synced'
  } catch (error) {
    console.error('Failed to restore auto-sync review draft:', error)
  }
}

const runAutoSync = async () => {
  if (!canAutoSync.value || isAutoSyncing.value) {
    return
  }

  isAutoSyncing.value = true
  const syncTrack = autoSyncViewerTrack.value
  startManualAutoSyncRun(syncTrack)
  openAutoSyncViewer()
  try {
    const result = await invoke('auto_sync_track_lyrics', { trackId: props.trackId })
    finishManualAutoSyncRun(syncTrack, result)
    if (result.status === 'succeeded') {
      const parsedLines = parseLrcLines(result.generatedLrc || '')
      if (parsedLines.length > 0) {
        updateSyncedLines(parsedLines)
        activeTab.value = 'synced'
      }
      toast.success('Lyrics auto-synced')
    } else if (result.status === 'needs_review') {
      const parsedLines = parseLrcLines(result.generatedLrc || '')
      if (parsedLines.length === 0) {
        toast.error('Auto-sync result needs review, but no valid timestamped lines were returned')
        return
      }

      replaceSyncedLines(parsedLines, { markDirty: false })
      autoSyncPreview.value = {
        id: result.id,
        generatedLrc: result.generatedLrc || '',
        confidence: result.confidence,
      }
      activeTab.value = 'synced'
      toast.info('Auto-sync result needs review')
    } else {
      toast.error(result.errorMessage || 'Auto-sync failed')
    }
  } catch (error) {
    failManualAutoSyncRun(syncTrack, error)
    console.error(error)
    toast.error(`Auto-sync failed: ${error}`)
  } finally {
    isAutoSyncing.value = false
  }
}

const applyAutoSyncPreview = async () => {
  if (!autoSyncPreview.value) {
    return
  }

  try {
    await invoke('apply_sync_result_to_lyricsfile', { syncId: autoSyncPreview.value.id })
    await saveLyrics()
    autoSyncPreview.value = null
    activeTab.value = 'synced'
    toast.success('Auto-sync result applied')
  } catch (error) {
    console.error(error)
    toast.error(`Failed to apply auto-sync result: ${error}`)
  }
}

const discardAutoSyncPreview = () => {
  autoSyncPreview.value = null
  initializeLyrics()
  activeTab.value = 'plain'
}

watch(activeTab, value => {
  if (value !== 'synced') {
    isSyncedLineEditing.value = false
    return
  }

  ensureSelectedSyncedLine()
})

const { bindSyncedHotkeys, unbindSyncedHotkeys } = useEditLyricsV2SyncedHotkeys({
  activeTab,
  isSyncedLineEditing,
  selectedLineExists,
  selectedSyncedLineIndex,
  syncedLines,
  selectSyncedLine,
  syncLineToCurrentProgress,
  rewindLineBy100: rewindLineTimestampBy100,
  forwardLineBy100: forwardLineTimestampBy100,
})

const changeCodemirrorFontSizeBy = offset => {
  const nextFontSize = Math.max(0.4, codemirrorStyle.value.fontSize + offset * 0.1)
  codemirrorStyle.value.fontSize = +nextFontSize.toFixed(2)
}

const resetCodemirrorFontSize = () => {
  codemirrorStyle.value.fontSize = 1.0
}

const debugModalContent = computed(() => {
  return serializedLyricsfile.value || ''
})

const { open: openDebugModal, close: closeDebugModal } = useModal({
  component: EditLyricsV2DebugModal,
  attrs: {
    content: debugModalContent,
    onClose() {
      closeDebugModal()
    },
  },
})

const { open: openConfirmModal, close: closeConfirmModal } = useModal({
  component: ConfirmModal,
  attrs: {
    title: 'Unsaved Changes',
    message: 'You have unsaved changes. Are you sure you want to close?',
    confirmText: 'Discard Changes',
    cancelText: 'Cancel',
    onConfirm() {
      closeConfirmModal()
      emit('close')
    },
    onCancel() {
      closeConfirmModal()
    },
  },
})

const handleClose = () => {
  if (isDirty.value) {
    openConfirmModal()
  } else {
    emit('close')
  }
}

const modalTitle = computed(() => {
  const title =
    audioSourceRef.value?.title || lyricsfileRef.value?.metadata?.title || 'Unknown Title'
  const artist =
    audioSourceRef.value?.artist_name || lyricsfileRef.value?.metadata?.artist || 'Unknown Artist'
  return `${title} - ${artist}`
})

const { bindHotkeys, unbindHotkeys } = useEditLyricsV2Hotkeys({
  activeTab,
  saveLyrics,
  changeFontSizeBy: changeCodemirrorFontSizeBy,
  resetFontSize: resetCodemirrorFontSize,
})

onMounted(async () => {
  disableHotkey()

  // Initialize lyrics from props
  initializeLyrics()
  await restoreAutoSyncReviewDraft()

  bindHotkeys()
  bindSyncedHotkeys()
})

onUnmounted(() => {
  clearLinePreview()
  unbindSyncedHotkeys()
  unbindHotkeys()
  enableHotkey()
})
</script>
