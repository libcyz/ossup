/* 模态弹窗。Prompt 带输入框（重命名 / 复制到 / 移动到 / 新建文件夹），
   Confirm 只确认（删除这类不可撤销的操作）。 */

import { useEffect, useRef, useState } from "react"

export type ConfirmSpec = {
	title: string
	body: string
	detail?: string
	confirmText: string
	onConfirm: () => void
}

export function Confirm({ spec, onClose }: { spec: ConfirmSpec; onClose: () => void }) {
	const ref = useRef<HTMLButtonElement>(null)


	useEffect(() => ref.current?.focus(), [])

	return (
		<div className="modal-scrim" onMouseDown={onClose}>
			<div
				className="modal"
				onMouseDown={(e) => e.stopPropagation()}
				onKeyDown={(e) => {
					if (e.key === "Escape") onClose()
				}}
				role="alertdialog"
				aria-modal="true"
			>
				<h3>{spec.title}</h3>
				<p style={{ margin: 0, fontSize: "var(--text-md)" }}>{spec.body}</p>
				{spec.detail && <p className="hint">{spec.detail}</p>}
				<div className="modal-actions">
					<button type="button" className="btn" ref={ref} onClick={onClose}>
						取消
					</button>
					<button
						type="button"
						className="btn danger"
						onClick={() => {
							onClose()
							spec.onConfirm()
						}}
					>
						{spec.confirmText}
					</button>
				</div>
			</div>
		</div>
	)
}

export type PromptSpec = {
	title: string
	label: string
	value: string
	hint?: string
	confirmText?: string
	/** 常用值的快捷按钮，点一下填进输入框 */
	presets?: { label: string; value: string }[]
	/** 返回错误文案表示不通过，返回空字符串表示放行 */
	validate?: (value: string) => string
	onConfirm: (value: string) => void
}

export function Prompt({ spec, onClose }: { spec: PromptSpec; onClose: () => void }) {
	const [value, setValue] = useState(spec.value)
	const [error, setError] = useState("")
	const ref = useRef<HTMLInputElement>(null)

	useEffect(() => {
		const el = ref.current
		if (!el) return
		el.focus()
		/* 重命名时只选中主干，扩展名留着不动 —— 和资源管理器一致 */
		const dot = spec.value.lastIndexOf(".")
		if (dot > 0 && spec.value.length - dot <= 8) el.setSelectionRange(0, dot)
		else el.select()
	}, [spec.value])

	const submit = (e: React.FormEvent) => {
		e.preventDefault()
		const trimmed = value.trim()
		if (!trimmed) return setError("不能为空")
		const problem = spec.validate?.(trimmed)
		if (problem) return setError(problem)
		onClose()
		spec.onConfirm(trimmed)
	}

	return (
		<div className="modal-scrim" onMouseDown={onClose}>
			<form className="modal" onMouseDown={(e) => e.stopPropagation()} onSubmit={submit}>
				<h3>{spec.title}</h3>
				<div className="field">
					<label htmlFor="prompt-input">{spec.label}</label>
					<input
						id="prompt-input"
						ref={ref}
						type="text"
						spellCheck={false}
						autoComplete="off"
						value={value}
						onChange={(e) => {
							setValue(e.target.value)
							setError("")
						}}
						onKeyDown={(e) => {
							if (e.key === "Escape") onClose()
						}}
					/>
					{spec.presets && (
						<div className="presets">
							{spec.presets.map((p) => (
								<button
									type="button"
									key={p.value}
									className={`preset ${value.trim() === p.value ? "active" : ""}`}
									onClick={() => {
										setValue(p.value)
										setError("")
									}}
								>
									{p.label}
								</button>
							))}
						</div>
					)}
					{spec.hint && <p className="hint">{spec.hint}</p>}
					{error && <p className="hint" style={{ color: "var(--red)" }}>{error}</p>}
				</div>
				<div className="modal-actions">
					<button type="button" className="btn" onClick={onClose}>
						取消
					</button>
					<button type="submit" className="btn primary">
						{spec.confirmText ?? "确定"}
					</button>
				</div>
			</form>
		</div>
	)
}
