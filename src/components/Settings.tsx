/* 传输参数设置。四个数字和直连开关都直接对应 ossutil 的行为，改完立即保存。 */

import { useState } from "react"

import { useSessionStore } from "../store/session.ts"
import { setMaxTasks } from "../store/transfers.ts"
import { useUiStore } from "../store/ui.ts"

type Field = {
	key: "maxTasks" | "jobs" | "parallel" | "partSizeMb"
	label: string
	min: number
	max: number
	hint: string
}

const FIELDS: Field[] = [
	{
		key: "maxTasks",
		label: "同时进行的任务数",
		min: 1,
		max: 10,
		hint: "每个任务是一个独立的 ossutil 进程。超出的任务排队等待，而不是一起抢带宽。",
	},
	{
		key: "jobs",
		label: "文件并发",
		min: 1,
		max: 64,
		hint: "单个任务内同时传几个文件（ossutil -j）。小文件多时调大。",
	},
	{
		key: "parallel",
		label: "分片并发",
		min: 1,
		max: 64,
		hint: "单个大文件切成多片后同时传几片（ossutil --parallel）。大文件时调大。",
	},
	{
		key: "partSizeMb",
		label: "分片大小（MB）",
		min: 1,
		max: 1024,
		hint: "超过 100MB 的文件才会分片。4K 视频建议 32–64。",
	},
]

export function Settings({ onClose }: { onClose: () => void }) {
	const config = useSessionStore((s) => s.config)
	const setConfig = useSessionStore((s) => s.setConfig)
	const saveConfig = useSessionStore((s) => s.saveConfig)
	const showToast = useUiStore((s) => s.showToast)

	const [draft, setDraft] = useState(() => ({
		maxTasks: config.maxTasks,
		jobs: config.jobs,
		parallel: config.parallel,
		partSizeMb: config.partSizeMb,
		direct: config.direct,
	}))

	const submit = (e: React.FormEvent) => {
		e.preventDefault()
		/* 逐项夹到合法区间：输入框能手打，别让 0 或负数进到命令行里 */
		const clean = { ...draft }
		for (const f of FIELDS) {
			const v = Math.round(Number(clean[f.key]))
			clean[f.key] = Number.isFinite(v) ? Math.max(f.min, Math.min(f.max, v)) : f.min
		}
		setConfig(clean)
		/* 队列的并发上限住在模块里，store 更新后要同步过去 */
		setMaxTasks(clean.maxTasks)
		void saveConfig()
		showToast("设置已保存", "success")
		onClose()
	}

	return (
		<div className="modal-scrim" onMouseDown={onClose}>
			<form className="modal settings" onMouseDown={(e) => e.stopPropagation()} onSubmit={submit}>
				<h3>设置</h3>

				{FIELDS.map((f) => (
					<div className="field" key={f.key}>
						<label htmlFor={f.key}>{f.label}</label>
						<input
							id={f.key}
							type="number"
							min={f.min}
							max={f.max}
							value={draft[f.key]}
							onChange={(e) =>
								setDraft((d) => ({ ...d, [f.key]: e.target.valueAsNumber }))
							}
						/>
						<p className="hint">{f.hint}</p>
					</div>
				))}

				<div className="field">
					<label className="check">
						<input
							type="checkbox"
							checked={draft.direct}
							onChange={(e) => setDraft((d) => ({ ...d, direct: e.target.checked }))}
						/>
						<span>绕过代理直连 OSS</span>
					</label>
					<p className="hint">
						Clash 等 TUN / 全局代理会把 OSS 流量绕到代理节点，速度被节点卡住。开启后从物理网卡直连；只能经代理访问 OSS 的网络请关闭。
					</p>
				</div>

				<div className="modal-actions">
					<button type="button" className="btn" onClick={onClose}>
						取消
					</button>
					<button type="submit" className="btn primary">
						保存
					</button>
				</div>
			</form>
		</div>
	)
}
