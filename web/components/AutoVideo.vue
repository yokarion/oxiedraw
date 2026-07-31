<script setup lang="ts">
// Autoplaying, muted, looping video for use inside Markdown/MDC content.
// Resolves src against the app baseURL so it works under the GitHub Pages subpath.
const props = withDefaults(defineProps<{
  src: string
  maxWidth?: string
}>(), {
  maxWidth: '300px',
})

const config = useRuntimeConfig()
const resolvedSrc = computed(() => {
  if (/^https?:\/\//.test(props.src)) return props.src
  const base = (config.app.baseURL || '/').replace(/\/$/, '')
  return `${base}/${props.src.replace(/^\//, '')}`
})

// Set muted via the property (the bare `muted` attribute is unreliable in Vue),
// otherwise browsers block autoplay.
const videoEl = ref<HTMLVideoElement>()
onMounted(() => {
  if (videoEl.value) videoEl.value.muted = true
})
</script>

<template>
  <video
    ref="videoEl"
    :src="resolvedSrc"
    :style="{ maxWidth }"
    autoplay
    muted
    loop
    playsinline
    preload="auto"
  />
</template>

<style scoped>
video {
  display: block;
  width: 100%;
  margin-inline: auto;
  border-radius: 6px;
}
</style>
