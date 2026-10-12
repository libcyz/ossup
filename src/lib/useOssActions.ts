/* 复制 / 粘贴 / 重命名 / 共享 / 删除 / 下载 / 打开 / 新建文件夹。
   工具栏和右键菜单调的是同一份，弹窗和剪贴板都放在 store 里共享。 */

import { openUrl } from "@tauri-apps/plugin-opener"
import { open as pickPath } from "@tauri-apps/plugin-dialog"
import { useState } from "react"

import { MAX_EXPIRES_SECONDS, parseDuration } from "./format.ts"
import type { OssItem } from "./objects.ts"
import { friendlyError } from "./ossApi.ts"
import { copyTo, download, makeDir, moveTo, presign, remove, uriOf } from "./ossOps.ts"
import { splitPath, useExplorerStore } from "../store/explorer.ts"
import { useSessionStore } from "../store/session.ts"
import { newTaskId } from "./transferReq.ts"
import { useTransfersStore } from "../store/transfers.ts"
import { useUiStore, type ClipItem } from "../store/ui.ts"

/** 名字冲突时补 (2)(3)…，扩展名留在最后：报告.json -> 报告 (2).json */
export function uniqueName(name: string, taken: Set<string>): string {
	if (!taken.has(name)) return name
	const dot = name.lastIndexOf(".")
	const hasExt = dot > 0 && name.length - dot <= 8
	const stem = hasExt ? name.slice(0, dot) : name
	const ext = hasExt ? name.slice(dot) : ""
	for (let i = 2; i < 1000; i++) {
		const candidate = `${stem} (${i})${ext}`
		if (!taken.has(candidate)) return candidate
	}
	return `${stem} (${Date.now()})${ext}`
}

export function useOssActions() {
	const [busy, setBusy] = useState(false)

	const currentPath = useExplorerStore((s) => s.currentPath)
	const items = useExplorerStore((s) => s.items)
	const selectedIds = useExplorerStore((s) => s.selectedIds)
	const navigate = useExplorerStore((s) => s.navigate)
	const refresh = useExplorerStore((s) => s.refresh)

	const session = useSessionStore()
	const { showToast, showPrompt, showConfirm, clipboard, setClipboard } = useUiStore()

	const { bucket, prefix } = splitPath(currentPath)
	const auth = session.auth()
	const dir = prefix ? `${prefix}/` : ""

	const selected = items.filter((o) => selectedIds.has(o.key))
	const one = selected.length === 1 ? selected[0] : null
	const taken = new Set(items.map((o) => o.name))

	/** 当前 Bucket 的地域，粘贴时用来拦跨地域复制 */
	const locationOf = (name: string) =>
		session.buckets.find((b) => b.name === name)?.location ?? ""

	const srcOf = (item: Pick<OssItem, "key" | "folder">) => ({
		uri: uriOf(bucket, item.key),
		folder: item.folder,
	})

	/* 统一：防重入 + 人话错误 + 完成后刷新 */
	const run = async (label: string, fn: () => Promise<unknown>, reload = true) => {
		if (busy) return
		setBusy(true)
		try {
			await fn()
			if (reload) await refresh()
		} catch (err) {
			session.appendLog(`[${label}] ${err}`)
			showToast(`${label}失败：${friendlyError(err)}`, "error")
		} finally {
			setBusy(false)
		}
	}

	const transferReq = (localPath: string) => ({
		localPath,
		bucket,
		prefix,
		accessKeyId: session.config.accessKeyId.trim(),
		accessKeySecret: session.config.accessKeySecret,
		endpoint: session.config.endpoint,
		jobs: session.config.jobs,
		parallel: session.config.parallel,
		partSizeMb: session.config.partSizeMb,
		ossutilPath: session.config.ossutilPath.trim(),
		cliCreds: session.config.cliCreds,
		direct: session.config.direct,
	})

	const validateUri = (value: string) =>
		/^oss:\/\/[^/\s]+\//.test(value.endsWith("/") ? value : `${value}/`)
			? ""
			: "格式是 oss://bucket/路径/"

	/* ---------------- 动作 ---------------- */

	const openItem = (item: OssItem) => {
		if (item.folder) {
			navigate(`${bucket}/${item.key.replace(/\/$/, "")}`)
			return
		}
		void run(
			"打开",
			async () => openUrl(await presign(auth, uriOf(bucket, item.key))),
			false,
		)
	}

	const downloadItem = (item: OssItem) => {
		void (async () => {
			const picked = await pickPath({ directory: true, multiple: false })
			const target = Array.isArray(picked) ? picked[0] : picked
			if (typeof target !== "string" || !target) return
			/* 这里只传选中的文件夹；真正写成「目录/原文件名」在后端，
			   否则 ossutil 会把目录本身当成目的文件（Linux 上再叠 -u 直接跳过）。
			   下载不走 run()：它是长任务，进度归状态栏管，
			   在这里 await 会把 busy 一直卡住、挡掉其它操作 */
			const id = newTaskId()
			useTransfersStore.getState().enqueue(
				{ id, kind: "download", label: item.name },
				async () => {
					try {
						await download(
							transferReq(target),
							uriOf(bucket, item.key),
							target,
							item.folder,
							id,
						)
					} catch (err) {
						useTransfersStore.getState().fail(id, String(err))
						session.appendLog(`[下载] ${err}`)
						showToast(`下载失败：${friendlyError(err)}`, "error")
					}
				},
			)
			showToast("已加入传输队列，进度看底部状态栏", "success")
		})()
	}

	/**
	 * 重命名 = 复制一份再删原件（OSS 没有 rename 这个 API）。
	 *
	 * 正因为要删原件，两种情况必须拦死，否则是实打实的数据丢失：
	 *  - 新名字和旧名字一样：会变成 cp x x 之后把 x 删掉，文件直接没了
	 *  - 新名字已被同目录里的别的对象占用：cp -f 会静默覆盖掉那个对象
	 */
	const renameItem = (item: OssItem) =>
		showPrompt({
			title: "重命名",
			label: "新名称",
			value: item.name,
			confirmText: "重命名",
			hint: "OSS 没有重命名接口，实际是复制一份再删除原件",
			validate: (v) => {
				if (v.includes("/")) return "名称里不能有斜杠"
				if (v === item.name) return "和原来的名字一样"
				if (taken.has(v)) return `「${v}」已存在，换个名字`
				return ""
			},
			onConfirm: (name) => {
				const dst = `oss://${bucket}/${dir}${name}${item.folder ? "/" : ""}`
				void run("重命名", () => moveTo(auth, srcOf(item), dst))
			},
		})

	const copyItemTo = (item: OssItem) =>
		showPrompt({
			title: "复制到",
			label: "目标路径",
			value: `oss://${currentPath}/`,
			hint: "服务端直接复制，不经过本机。ossutil 不支持跨地域复制。",
			confirmText: "复制",
			validate: validateUri,
			onConfirm: (dst) =>
				void run("复制", () =>
					copyTo(
						auth,
						srcOf(item),
						`${dst.replace(/\/+$/, "")}/${item.name}${item.folder ? "/" : ""}`,
					),
				),
		})

	const moveItemTo = (item: OssItem) =>
		showPrompt({
			title: "移动到",
			label: "目标路径",
			value: `oss://${currentPath}/`,
			hint: "先复制再删除原件，中途失败会留下副本",
			confirmText: "移动",
			validate: validateUri,
			onConfirm: (dst) =>
				void run("移动", () =>
					moveTo(
						auth,
						srcOf(item),
						`${dst.replace(/\/+$/, "")}/${item.name}${item.folder ? "/" : ""}`,
					),
				),
		})

	const shareItem = (item: OssItem) =>
		showPrompt({
			title: "生成临时链接",
			label: "有效期",
			value: "1h",
			confirmText: "生成并复制",
			hint: "写法如 30m / 1h / 1h30m / 7d，范围 1 秒 – 7 天（OSS v4 签名的上限）",
			presets: [
				{ label: "15 分钟", value: "15m" },
				{ label: "1 小时", value: "1h" },
				{ label: "6 小时", value: "6h" },
				{ label: "1 天", value: "1d" },
				{ label: "7 天", value: "7d" },
			],
			validate: (v) => {
				const seconds = parseDuration(v)
				if (seconds === null) return "认不出这个时长，例如 30m / 1h / 7d"
				if (seconds > MAX_EXPIRES_SECONDS) return "最长只能 7 天"
				return ""
			},
			onConfirm: (duration) =>
				void run(
					"生成链接",
					async () => {
						const url = await presign(auth, uriOf(bucket, item.key), duration)
						await navigator.clipboard.writeText(url)
						showToast(`链接已复制，${duration} 内有效`, "success")
					},
					false,
				),
		})

	const deleteItems = (list: OssItem[]) => {
		if (!list.length) return
		const only = list.length === 1 ? list[0] : null
		showConfirm({
			title: "删除",
			body: only
				? only.folder
					? `要删除文件夹「${only.name}」及其下全部对象吗？`
					: `要删除「${only.name}」吗？`
				: `要删除选中的 ${list.length} 项吗？`,
			detail: only
				? `oss://${bucket}/${only.key}\nOSS 上的删除不可撤销，除非这个 Bucket 开了版本控制。`
				: `${list
						.slice(0, 5)
						.map((o) => o.name)
						.join("、")}${list.length > 5 ? ` 等 ${list.length} 项` : ""}\nOSS 上的删除不可撤销，除非这个 Bucket 开了版本控制。`,
			confirmText: "永久删除",
			onConfirm: () =>
				void run("删除", async () => {
					for (const item of list) await remove(auth, srcOf(item))
				}),
		})
	}

	/* ---------------- 剪贴板 ---------------- */

	const copySelection = () => {
		if (!selected.length) return
		setClipboard({
			bucket,
			location: locationOf(bucket),
			items: selected.map<ClipItem>((o) => ({
				key: o.key,
				name: o.name,
				folder: o.folder,
			})),
		})
		showToast(`已复制 ${selected.length} 项，进目标目录点粘贴`, "success")
	}

	const canPaste = Boolean(clipboard?.items.length && bucket)

	/** ossutil cp 明确不支持跨地域，提前拦下来比让它报一句英文错强 */
	const pasteBlockedReason = (): string => {
		if (!clipboard) return ""
		const here = locationOf(bucket)
		if (clipboard.location && here && clipboard.location !== here) {
			return `不能跨地域复制：来源在 ${clipboard.location}，当前在 ${here}`
		}
		return ""
	}

	const paste = () => {
		if (!clipboard || !bucket) return
		const blocked = pasteBlockedReason()
		if (blocked) {
			showToast(blocked, "error")
			return
		}
		/* 粘回同一个目录时自动补 (2)，不覆盖同名对象 */
		const used = new Set(taken)
		const jobs = clipboard.items.map((item) => {
			const name = uniqueName(item.name, used)
			used.add(name)
			return {
				src: { uri: uriOf(clipboard.bucket, item.key), folder: item.folder },
				dst: `oss://${bucket}/${dir}${name}${item.folder ? "/" : ""}`,
			}
		})
		void run("粘贴", async () => {
			for (const job of jobs) await copyTo(auth, job.src, job.dst)
			showToast(`已粘贴 ${jobs.length} 项`, "success")
		})
	}

	const newFolder = () =>
		showPrompt({
			title: "新建文件夹",
			label: "文件夹名",
			value: uniqueName("新建文件夹", taken),
			confirmText: "创建",
			validate: (v) => {
				if (v.includes("/")) return "名称里不能有斜杠"
				if (taken.has(v)) return `「${v}」已存在`
				return ""
			},
			onConfirm: (name) =>
				void run("新建文件夹", () => makeDir(auth, `oss://${bucket}/${dir}${name}/`)),
		})

	return {
		busy,
		bucket,
		selected,
		one,
		canPaste,
		clipboardCount: clipboard?.items.length ?? 0,
		openItem,
		downloadItem,
		renameItem,
		copyItemTo,
		moveItemTo,
		shareItem,
		deleteItems,
		copySelection,
		paste,
		newFolder,
	}
}
