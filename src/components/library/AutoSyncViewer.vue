<template>
  <BaseModal
    content-class="w-full h-[80vh] max-w-screen-md"
    body-class="flex flex-col h-full min-h-0 justify-between gap-6"
    :title="isFinished ? 'Auto-sync Complete' : 'Auto-syncing Lyrics'"
    @close="checkAndClose"
  >
    <div class="flex flex-col items-center justify-center gap-1">
      <div class="w-full bg-neutral-50 h-1 rounded">
        <div class="bg-hoa-1100 h-1" :style="{ width: progressWidth }" />
      </div>
      <div class="text-[0.7rem] text-neutral-500 dark:text-neutral-500 flex gap-3">
        <span>{{ syncedCount }} SYNCED</span>
        <span>{{ reviewCount }} REVIEW</span>
        <span>{{ failureCount }} FAILED</span>
        <span>{{ totalCount }} TOTAL</span>
      </div>
      <div v-if="latestEvent" class="text-xs text-neutral-600 dark:text-neutral-400">
        {{ latestEvent.title }} - {{ latestEvent.artistName }} · {{ latestEvent.phase }}
      </div>
    </div>

    <div class="rounded-lg p-3 bg-white dark:bg-neutral-950 w-full text-xs grow overflow-auto">
      <div v-if="combinedLog.length === 0" class="text-neutral-500 dark:text-neutral-500">
        Waiting for auto-sync output...
      </div>
      <div
        v-for="(logItem, index) in combinedLog"
        :key="`${logItem.createdAt}-${index}`"
        class="mb-1"
        :class="{
          'text-green-800 dark:text-green-400':
            logItem.status === 'synced' || logItem.status === 'finished',
          'text-yellow-800 dark:text-yellow-400':
            logItem.status === 'review' || logItem.status === 'started',
          'text-red-800 dark:text-red-400':
            logItem.status === 'failure' || logItem.status === 'failed',
          'text-neutral-700 dark:text-neutral-300': logItem.status === 'log',
        }"
      >
        <strong>{{ logItem.title }} - {{ logItem.artistName }}</strong
        >:
        <span v-if="logItem.phase">[{{ logItem.phase }}] </span>
        <span v-if="logItem.stream">[{{ logItem.stream }}] </span>
        <span>{{ logItem.message }}</span>
        <span v-if="Number.isFinite(logItem.elapsedMs)">
          ({{ (logItem.elapsedMs / 1000).toFixed(1) }}s)
        </span>
      </div>
    </div>

    <template #footer>
      <div class="flex-none flex justify-center">
        <button
          v-if="isFinished"
          class="button button-primary px-8 py-2 rounded-full"
          @click="checkAndClose"
        >
          Finish
        </button>
        <button v-else class="button button-normal px-8 py-2 rounded-full" @click="handleStop">
          Stop queue
        </button>
      </div>
    </template>
  </BaseModal>
</template>

<script setup>
import { computed, onUnmounted } from 'vue'
import BaseModal from '@/components/common/BaseModal.vue'
import { useAutoSync } from '@/composables/auto-sync.js'

const {
  isAutoSyncing,
  autoSyncProgress,
  syncedCount,
  reviewCount,
  failureCount,
  processedCount,
  totalCount,
  log,
  engineLog,
  startOver,
  stopAutoSyncing,
} = useAutoSync()

const emit = defineEmits(['close'])

const progressWidth = computed(() => {
  if (!isAutoSyncing.value) {
    return '100%'
  }
  if (autoSyncProgress.value >= 1.0) {
    return '100%'
  }
  return `${autoSyncProgress.value * 100}%`
})

const isFinished = computed(() => {
  if (!isAutoSyncing.value) {
    return true
  }
  if (totalCount.value === 0) {
    return false
  }
  return processedCount.value >= totalCount.value
})

const normalizedQueueLog = computed(() =>
  log.value.map(item => ({
    ...item,
    createdAt: item.createdAt || 0,
  }))
)

const combinedLog = computed(() =>
  [...engineLog.value, ...normalizedQueueLog.value].sort(
    (left, right) => (right.createdAt || 0) - (left.createdAt || 0)
  )
)

const latestEvent = computed(() => engineLog.value[engineLog.value.length - 1] || null)

const handleStop = () => {
  stopAutoSyncing()
  emit('close')
}

const checkAndClose = () => {
  if (isFinished.value) {
    startOver()
    emit('close')
  } else {
    emit('close')
  }
}

onUnmounted(() => {
  checkAndClose()
})
</script>
