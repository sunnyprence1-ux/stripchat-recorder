<!--
    添加主播对话框组件 / Add Streamer Dialog Component

    提供一个模态对话框，允许用户通过输入 Stripchat 用户名来添加新主播到追踪列表。
    提交时调用 streamersStore.addStreamer 并通过 toast 反馈操作结果。

    Provides a modal dialog for users to add a new streamer to the tracking list
    by entering their Stripchat username. Calls streamersStore.addStreamer on submit
    and provides toast feedback for the operation result.

    Emits:
        close - 对话框关闭时触发 / Emitted when dialog is closed
        added - 主播成功添加后触发 / Emitted after streamer is successfully added
-->
<script setup lang="ts">
	import { computed, onMounted, onUnmounted, ref } from "vue";
	import { useStreamersStore } from "../stores/streamers";
	import { useNotify } from "../composables/useNotify";
	import {
		Dialog,
		DialogContent,
		DialogDescription,
		DialogHeader,
		DialogTitle,
		DialogFooter,
	} from "@/components/ui/dialog";
	import { Button } from "@/components/ui/button";
	import { Input } from "@/components/ui/input";
	import { Label } from "@/components/ui/label";
	import { useI18n } from "vue-i18n";

	const emit = defineEmits<{ close: []; added: [] }>();
	const store = useStreamersStore();
	const { toast } = useNotify();
	const { t } = useI18n();
	const username = ref("");
	const loading = ref(false);

	type LiveRoom = { username: string; thumbnail_url: string | null };
	const tab = ref<"browse" | "manual">("browse");
	const rooms = ref<LiveRoom[]>([]);
	const nextOffset = ref<number | null>(0);
	const browsing = ref(false);
	const browseError = ref("");
	const addingRoom = ref<string | null>(null);
	const brokenImages = ref(new Set<string>());
	const tracked = computed(() => new Set(store.streamers.map((entry) => entry.username.toLowerCase())));
	const controller = new AbortController();

	async function loadRooms() {
		const offset = nextOffset.value;
		if (browsing.value || offset === null) return;
		browsing.value = true;
		browseError.value = "";
		try {
			const response = await fetch(`/api/live-rooms?offset=${offset}`, {
				signal: controller.signal,
			});
			if (!response.ok) throw new Error(await response.text());
			const page = await response.json() as { rooms: LiveRoom[]; next_offset: number | null };
			const seen = new Set(rooms.value.map((room) => room.username.toLowerCase()));
			for (const room of page.rooms) {
				const name = room.username.toLowerCase();
				if (seen.has(name)) continue;
				seen.add(name);
				rooms.value.push(room);
			}
			nextOffset.value = page.next_offset;
		} catch {
			if (!controller.signal.aborted) browseError.value = "Could not load live rooms. Try again or add a room manually.";
		} finally {
			browsing.value = false;
		}
	}

	async function addRoom(room: LiveRoom) {
		const name = room.username.toLowerCase();
		if (addingRoom.value || tracked.value.has(name)) return;
		addingRoom.value = name;
		try {
			await store.addStreamer(name);
			toast(t("addStreamer.done", { name }), "success");
		} catch (error) {
			toast(String(error), "error");
		} finally {
			addingRoom.value = null;
		}
	}

	onMounted(loadRooms);
	onUnmounted(() => controller.abort());

	/**
	 * 从输入中提取用户名：支持直接输入用户名、stripchat.com 链接或 mirror 链接。
	 * Extract username from input: supports plain username, stripchat.com URL, or mirror URL.
	 */
	function extractUsername(input: string): string {
		const trimmed = input.trim();
		try {
			const url = new URL(trimmed.startsWith("http") ? trimmed : `https://${trimmed}`);
			// 匹配 stripchat.com 或任意 mirror 域名下的 /<username> 路径
			const parts = url.pathname.split("/").filter(Boolean);
			if (parts.length > 0) return parts[0];
		} catch {
			// 不是 URL，直接当用户名
		}
		return trimmed;
	}

	/**
	 * 提交表单：验证输入、调用 addStreamer、反馈结果。
	 * Submit the form: validate input, call addStreamer, provide feedback.
	 */
	async function submit() {
		const name = extractUsername(username.value);
		if (!name) return;
		loading.value = true;
		try {
			await store.addStreamer(name);
			toast(t("addStreamer.done", { name }), "success");
			emit("added");
		} catch (e) {
			toast(String(e), "error");
		} finally {
			loading.value = false;
		}
	}
</script>

<template>
	<Dialog :open="true" @update:open="(v) => !v && emit('close')">
		<DialogContent class="sm:max-w-2xl dialog-max-h overflow-y-auto">
			<DialogHeader>
				<DialogTitle>{{ t("addStreamer.title") }}</DialogTitle>
				<DialogDescription class="sr-only">{{ t("addStreamer.description") }}</DialogDescription>
			</DialogHeader>

			<div class="flex flex-wrap gap-2" role="group" aria-label="Add method">
				<Button type="button" :variant="tab === 'browse' ? 'default' : 'outline'" :aria-pressed="tab === 'browse'" @click="tab = 'browse'">Browse live rooms</Button>
				<Button type="button" :variant="tab === 'manual' ? 'default' : 'outline'" :aria-pressed="tab === 'manual'" :disabled="!!addingRoom" @click="tab = 'manual'">Add manually</Button>
			</div>

			<section v-if="tab === 'browse'" aria-label="Live rooms" class="grid gap-3">
				<div v-if="rooms.length" class="grid grid-cols-2 gap-2 sm:grid-cols-3">
					<article v-for="room in rooms" :key="room.username" class="min-w-0 overflow-hidden rounded-md border bg-card">
						<div class="grid aspect-video place-items-center overflow-hidden bg-muted">
							<img v-if="room.thumbnail_url && !brokenImages.has(room.username)" :src="room.thumbnail_url"
								alt="" loading="lazy" decoding="async" referrerpolicy="no-referrer"
								class="size-full object-cover" @error="brokenImages.add(room.username)" />
							<span v-else aria-hidden="true" class="font-mono text-muted-foreground">{{ room.username.slice(0, 2).toUpperCase() }}</span>
						</div>
						<div class="grid gap-2 p-2">
							<h3 class="break-all text-sm font-semibold">{{ room.username }}</h3>
							<Button size="sm" :disabled="!!addingRoom || tracked.has(room.username.toLowerCase())"
								:aria-label="'Add ' + room.username" @click="addRoom(room)">
								{{ tracked.has(room.username.toLowerCase()) ? 'Added' : addingRoom === room.username.toLowerCase() ? 'Adding…' : 'Add' }}
							</Button>
						</div>
					</article>
				</div>
				<p v-if="browseError" role="alert" class="text-sm text-muted-foreground">{{ browseError }}</p>
				<p v-else-if="!browsing && !rooms.length" role="status" class="text-sm text-muted-foreground">No live rooms on this page.</p>
				<Button v-if="nextOffset !== null" type="button" variant="outline" :disabled="browsing" @click="loadRooms">
					{{ browsing ? 'Loading live rooms…' : browseError ? 'Retry' : 'Load more' }}
				</Button>
			</section>

			<form v-if="tab === 'manual'" @submit.prevent="submit" class="flex flex-col gap-4 py-2">
				<div class="flex flex-col gap-2">
					<Label for="username">{{ t("addStreamer.label") }}</Label>
				<Input
						id="username"
						v-model="username"
						:placeholder="t('addStreamer.placeholder')"
						:disabled="loading"
					/>
				</div>

				<DialogFooter>
					<Button type="button" variant="outline" @click="emit('close')">
						{{ t("addStreamer.cancel") }}
					</Button>
					<Button type="submit" :disabled="loading || !username.trim()">
						{{ loading ? t("addStreamer.submitting") : t("addStreamer.submit") }}
					</Button>
				</DialogFooter>
			</form>
		</DialogContent>
	</Dialog>
</template>

