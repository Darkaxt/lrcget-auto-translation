<template>
  <BaseModal
    title="Configuration"
    body-class="flex flex-col h-full justify-between overflow-y-auto"
    @before-open="beforeOpenHandler"
    @close="emit('close')"
  >
    <div class="flex flex-col gap-8">
      <div>
        <label class="group-label mb-4">Common</label>

        <div class="flex flex-col mb-4">
          <label class="block mb-2 child-label">Download lyrics for:</label>

          <RadioButton
            id="download-lyrics-for-all"
            v-model="downloadLyricsFor"
            class="mb-1"
            name="download-lyrics-for"
            value="all"
          >
            All tracks (overwrite existing lyrics)
          </RadioButton>

          <RadioButton
            id="skip-synced"
            v-model="downloadLyricsFor"
            class="mb-1"
            name="download-lyrics-for"
            value="skipSynced"
          >
            Only tracks without synced lyrics
          </RadioButton>

          <RadioButton
            id="skip-plain"
            v-model="downloadLyricsFor"
            class="mb-1"
            name="download-lyrics-for"
            value="skipPlain"
          >
            Only tracks without any lyrics
          </RadioButton>
        </div>

        <!-- Total lines number should always show in search result, this configuration is not necessary -->
        <!-- <div class="flex flex-col mb-4">
          <label class="block mb-2 child-label">Search settings</label>

          <CheckboxButton id="show-line-count" v-model="showLineCount" name="show-line-count">
            Show the number of lines a lyric file has in the search menu
          </CheckboxButton>
        </div> -->

        <div class="flex flex-col mb-4">
          <label class="block mb-2 child-label">Theme mode</label>

          <div class="flex gap-4">
            <RadioButton id="theme-auto" v-model="editingThemeMode" name="theme-mode" value="auto">
              Auto
            </RadioButton>

            <RadioButton
              id="theme-light"
              v-model="editingThemeMode"
              name="theme-mode"
              value="light"
            >
              Light
            </RadioButton>

            <RadioButton id="theme-dark" v-model="editingThemeMode" name="theme-mode" value="dark">
              Dark
            </RadioButton>
          </div>
        </div>

        <div class="flex flex-col">
          <label class="block mb-2 child-label" for="lrclib-instance">LRCLIB instance</label>
          <input
            id="lrclib-instance"
            v-model="editingLrclibInstance"
            type="text"
            placeholder="https://"
            class="input px-4 h-8"
          />
        </div>
      </div>

      <div>
        <label class="group-label mb-4">Translation</label>

        <div class="flex items-start mb-4">
          <CheckboxButton
            id="translation-auto-enabled"
            v-model="translationAutoEnabled"
            name="translation-auto-enabled"
          >
            Auto-translate lyrics after download
          </CheckboxButton>
        </div>

        <div class="grid grid-cols-2 gap-4 mb-4">
          <div class="flex flex-col">
            <label class="block mb-2 child-label" for="translation-provider">Provider</label>
            <select id="translation-provider" v-model="translationProvider" class="input px-3 h-8">
              <option value="gemini">Gemini Flash</option>
              <option value="deepl">DeepL</option>
              <option value="google">Google Cloud Translate</option>
              <option value="microsoft">Microsoft Translator</option>
              <option value="openai_compatible">OpenAI-compatible endpoint</option>
            </select>
          </div>

          <div class="flex flex-col">
            <label class="block mb-2 child-label" for="translation-target-language"
              >Target language</label
            >
            <input
              id="translation-target-language"
              v-model="translationTargetLanguage"
              type="text"
              class="input px-4 h-8"
              placeholder="English"
            />
          </div>
        </div>

        <div class="flex flex-col mb-4">
          <label class="block mb-2 child-label">Export lyrics as</label>
          <div class="flex gap-4 flex-wrap">
            <RadioButton
              id="translation-export-original"
              v-model="translationExportMode"
              name="translation-export-mode"
              value="original"
            >
              Original
            </RadioButton>
            <RadioButton
              id="translation-export-translated"
              v-model="translationExportMode"
              name="translation-export-mode"
              value="translation"
            >
              Translation
            </RadioButton>
            <RadioButton
              id="translation-export-dual"
              v-model="translationExportMode"
              name="translation-export-mode"
              value="dual"
            >
              Dual timestamp
            </RadioButton>
          </div>
        </div>

        <div v-if="translationProvider === 'gemini'" class="flex flex-col gap-3">
          <div class="flex flex-col">
            <label class="block mb-2 child-label" for="translation-gemini-api-key"
              >Gemini API key</label
            >
            <input
              id="translation-gemini-api-key"
              v-model="translationGeminiApiKey"
              type="password"
              class="input px-4 h-8"
            />
          </div>
          <CheckboxButton
            id="translation-gemini-advanced"
            v-model="showAdvancedGeminiModel"
            name="translation-gemini-advanced"
          >
            Use custom Gemini model
          </CheckboxButton>
          <div v-if="showAdvancedGeminiModel" class="flex flex-col">
            <label class="block mb-2 child-label" for="translation-gemini-model"
              >Gemini model</label
            >
            <input
              id="translation-gemini-model"
              v-model="translationGeminiModel"
              type="text"
              class="input px-4 h-8"
              placeholder="gemini-flash-latest"
            />
          </div>
        </div>

        <div v-else-if="translationProvider === 'deepl'" class="flex flex-col">
          <label class="block mb-2 child-label" for="translation-deepl-api-key"
            >DeepL API key</label
          >
          <input
            id="translation-deepl-api-key"
            v-model="translationDeeplApiKey"
            type="password"
            class="input px-4 h-8"
          />
        </div>

        <div v-else-if="translationProvider === 'google'" class="flex flex-col">
          <label class="block mb-2 child-label" for="translation-google-api-key"
            >Google Cloud API key</label
          >
          <input
            id="translation-google-api-key"
            v-model="translationGoogleApiKey"
            type="password"
            class="input px-4 h-8"
          />
        </div>

        <div v-else-if="translationProvider === 'microsoft'" class="grid grid-cols-2 gap-4">
          <div class="flex flex-col">
            <label class="block mb-2 child-label" for="translation-microsoft-api-key"
              >Microsoft API key</label
            >
            <input
              id="translation-microsoft-api-key"
              v-model="translationMicrosoftApiKey"
              type="password"
              class="input px-4 h-8"
            />
          </div>
          <div class="flex flex-col">
            <label class="block mb-2 child-label" for="translation-microsoft-region">Region</label>
            <input
              id="translation-microsoft-region"
              v-model="translationMicrosoftRegion"
              type="text"
              class="input px-4 h-8"
              placeholder="global or resource region"
            />
          </div>
        </div>

        <div v-else-if="translationProvider === 'openai_compatible'" class="flex flex-col gap-3">
          <div class="grid grid-cols-2 gap-4">
            <div class="flex flex-col">
              <label class="block mb-2 child-label" for="translation-openai-base-url"
                >Base URL</label
              >
              <input
                id="translation-openai-base-url"
                v-model="translationOpenaiBaseUrl"
                type="text"
                class="input px-4 h-8"
                placeholder="http://localhost:11434/v1"
              />
            </div>
            <div class="flex flex-col">
              <label class="block mb-2 child-label" for="translation-openai-model">Model</label>
              <input
                id="translation-openai-model"
                v-model="translationOpenaiModel"
                type="text"
                class="input px-4 h-8"
              />
            </div>
          </div>
          <div class="flex flex-col">
            <label class="block mb-2 child-label" for="translation-openai-api-key">API key</label>
            <input
              id="translation-openai-api-key"
              v-model="translationOpenaiApiKey"
              type="password"
              class="input px-4 h-8"
            />
          </div>
        </div>
      </div>

      <div>
        <label class="group-label mb-4">Experimental</label>

        <div class="flex items-start">
          <CheckboxButton id="try-embed-lyrics" v-model="tryEmbedLyrics" name="try-embed-lyrics">
            <div class="flex flex-col">
              <span class="mb-0.5">Enable embed lyrics option</span>
              <span class="text-xs text-yellow-700 dark:text-yellow-400"
                >This option could corrupt your track files. Make sure to backup your library before
                enabling it.</span
              >
            </div>
          </CheckboxButton>
        </div>
      </div>

      <div class="flex flex-col gap-1">
        <a href="#" class="link hidden" @click="refreshLibrary"
          >Scan for new and modified tracks...</a
        >
        <a href="#" class="link" @click="fullScanLibrary">Reset library and perform full scan...</a>
        <a href="#" class="link" @click="manageDirectories"
          >Add and remove scanning directories...</a
        >
      </div>
    </div>

    <template #footer>
      <button class="button button-primary px-8 py-2 rounded-full" @click="save">Save</button>
    </template>
  </BaseModal>
</template>

<script setup>
import { invoke } from '@tauri-apps/api/core'
import { ref, watch } from 'vue'
import { useGlobalState } from '../../composables/global-state'
import { usePlayer } from '@/composables/player.js'
import RadioButton from '@/components/common/RadioButton.vue'
import CheckboxButton from '@/components/common/CheckboxButton.vue'

const { setThemeMode, setLrclibInstance } = useGlobalState()
const { volume } = usePlayer()

const emit = defineEmits(['close', 'refreshLibrary', 'fullScanLibrary', 'manageDirectories'])

const downloadLyricsFor = ref('all')
const skipTracksWithSyncedLyrics = ref(true)
const skipTracksWithPlainLyrics = ref(false)
const showLineCount = ref(true)
const tryEmbedLyrics = ref(false)
const editingThemeMode = ref('auto')
const editingLrclibInstance = ref('')
const translationAutoEnabled = ref(false)
const translationTargetLanguage = ref('English')
const translationProvider = ref('gemini')
const translationExportMode = ref('original')
const translationGeminiApiKey = ref('')
const translationGeminiModel = ref('gemini-flash-latest')
const translationDeeplApiKey = ref('')
const translationGoogleApiKey = ref('')
const translationMicrosoftApiKey = ref('')
const translationMicrosoftRegion = ref('')
const translationOpenaiBaseUrl = ref('')
const translationOpenaiApiKey = ref('')
const translationOpenaiModel = ref('')
const showAdvancedGeminiModel = ref(false)

const save = async () => {
  await invoke('set_config', {
    skipTracksWithSyncedLyrics: skipTracksWithSyncedLyrics.value,
    skipTracksWithPlainLyrics: skipTracksWithPlainLyrics.value,
    showLineCount: showLineCount.value,
    tryEmbedLyrics: tryEmbedLyrics.value,
    themeMode: editingThemeMode.value,
    lrclibInstance: editingLrclibInstance.value,
    volume: volume.value,
    translationAutoEnabled: translationAutoEnabled.value,
    translationTargetLanguage: translationTargetLanguage.value,
    translationProvider: translationProvider.value,
    translationExportMode: translationExportMode.value,
    translationGeminiApiKey: translationGeminiApiKey.value,
    translationGeminiModel: showAdvancedGeminiModel.value
      ? translationGeminiModel.value
      : 'gemini-flash-latest',
    translationDeeplApiKey: translationDeeplApiKey.value,
    translationGoogleApiKey: translationGoogleApiKey.value,
    translationMicrosoftApiKey: translationMicrosoftApiKey.value,
    translationMicrosoftRegion: translationMicrosoftRegion.value,
    translationOpenaiBaseUrl: translationOpenaiBaseUrl.value,
    translationOpenaiApiKey: translationOpenaiApiKey.value,
    translationOpenaiModel: translationOpenaiModel.value,
  })
  setThemeMode(editingThemeMode.value)
  setLrclibInstance(editingLrclibInstance.value)
  emit('close')
}

const refreshLibrary = () => {
  emit('refreshLibrary')
  emit('close')
}

const fullScanLibrary = () => {
  emit('fullScanLibrary')
  emit('close')
}

const manageDirectories = () => {
  emit('manageDirectories')
  emit('close')
}

const beforeOpenHandler = async () => {
  const config = await invoke('get_config')
  skipTracksWithSyncedLyrics.value = config.skip_tracks_with_synced_lyrics
  skipTracksWithPlainLyrics.value = config.skip_tracks_with_plain_lyrics

  console.log(skipTracksWithSyncedLyrics.value, skipTracksWithPlainLyrics.value)

  if (skipTracksWithSyncedLyrics.value && !skipTracksWithPlainLyrics.value) {
    downloadLyricsFor.value = 'skipSynced'
  } else if (skipTracksWithPlainLyrics.value) {
    downloadLyricsFor.value = 'skipPlain'
  } else {
    downloadLyricsFor.value = 'all'
  }

  showLineCount.value = config.show_line_count
  tryEmbedLyrics.value = config.try_embed_lyrics
  editingThemeMode.value = config.theme_mode
  editingLrclibInstance.value = config.lrclib_instance
  translationAutoEnabled.value = config.translation_auto_enabled
  translationTargetLanguage.value = config.translation_target_language || 'English'
  translationProvider.value = config.translation_provider || 'gemini'
  translationExportMode.value = config.translation_export_mode || 'original'
  translationGeminiApiKey.value = config.translation_gemini_api_key || ''
  translationGeminiModel.value = config.translation_gemini_model || 'gemini-flash-latest'
  translationDeeplApiKey.value = config.translation_deepl_api_key || ''
  translationGoogleApiKey.value = config.translation_google_api_key || ''
  translationMicrosoftApiKey.value = config.translation_microsoft_api_key || ''
  translationMicrosoftRegion.value = config.translation_microsoft_region || ''
  translationOpenaiBaseUrl.value = config.translation_openai_base_url || ''
  translationOpenaiApiKey.value = config.translation_openai_api_key || ''
  translationOpenaiModel.value = config.translation_openai_model || ''
  showAdvancedGeminiModel.value = translationGeminiModel.value !== 'gemini-flash-latest'
}

watch(downloadLyricsFor, newVal => {
  if (newVal === 'skipSynced') {
    skipTracksWithSyncedLyrics.value = true
    skipTracksWithPlainLyrics.value = false
  } else if (newVal === 'skipPlain') {
    skipTracksWithSyncedLyrics.value = true
    skipTracksWithPlainLyrics.value = true
  } else {
    skipTracksWithSyncedLyrics.value = false
    skipTracksWithPlainLyrics.value = false
  }
})
</script>
