
import { useEffect, useLayoutEffect, useRef, useState } from "react"

import { useSize } from "../lib/hooks.ts"
import { pickObjects } from "../lib/objects.ts"
import { friendlyError, ossApi } from "../lib/ossApi.ts"
import {
	joinPath,
	parsePath,
	splitPath,
	useExplorerStore,
} from "../store/explorer.ts"
import { normalizeEndpoint, useSessionStore } from "../store/session.ts"
import { useUiStore } from "../store/ui.ts"
import { Menu, type MenuItem } from "./Menu.tsx"

export function Breadcrumb() {
	const breadcrumbs = useExplorerStore((s) => s.breadcrumbs)
	const currentPath = useExplorerStore((s) => s.currentPath)
	const navigate = useExplorerStore((s) => s.navigate)
	const setConfig = useSessionStore((s) => s.setConfig)
	const showToast = useUiStore((s) => s.showToast)

	const [editing, setEditing] = useState(false)
	const [draft, setDraft] = useState("")
	const [visibleCount, setVisibleCount] = useState(breadcrumbs.length)
	const [expanded, setExpanded] = useState(false)
	const [siblingMenu, setSiblingMenu] = useState<
		{ x: number; y: number; level: number } | null
	>(null)

	const { ref: boxRef, width } = useSize<HTMLDivElement>()
	const innerRef = useRef<HTMLDivElement>(null)
	const inputRef = useRef<HTMLInputElement>(null)

	/* 每次路径或宽度变化，先摊开全量再实测，塞不下就从中间往回收一级。
	   始终保留首级和末两级，所以最少留 3 级。 */
	useLayoutEffect(() => {
		setExpanded(false)
		setVisibleCount(breadcrumbs.length)
	}, [breadcrumbs.length, currentPath])

	useLayoutEffect(() => {
		if (editing || expanded) return
		const inner = innerRef.current
		if (!inner || !width) return
		if (inner.scrollWidth > width && visibleCount > 3) {
			setVisibleCount((n) => n - 1)
		}
	}, [width, visibleCount, breadcrumbs, editing, expanded])

	useEffect(() => {
		if (editing) inputRef.current?.select()
	}, [editing])

	const startEdit = () => {
		setDraft(currentPath ? `oss://${currentPath}/` : "oss://")
		setEditing(true)
	}

	const commit = () => {
		const parsed = parsePath(draft)
		if (!parsed) {
			showToast("认不出这条路径，格式是 oss://bucket/路径/", "error")
			return
		}
		if (parsed.endpoint) setConfig({ endpoint: normalizeEndpoint(parsed.endpoint) })
		setEditing(false)
		navigate(parsed.path)
	}

	if (editing) {
		return (
			<input
				ref={inputRef}
				className="addrinput"
				value={draft}
				spellCheck={false}
				onChange={(e) => setDraft(e.target.value)}
				onKeyDown={(e) => {
					if (e.key === "Enter") commit()
					if (e.key === "Escape") setEditing(false)
				}}
				onBlur={() => setEditing(false)}
			/>
		)
	}

	const collapsed = !expanded && visibleCount < breadcrumbs.length
	/* 首级 + … + 末两级；中间被折叠的那些进 … 的下拉 */
	const head = collapsed ? breadcrumbs.slice(0, 1) : breadcrumbs.slice(0, visibleCount - 2)
	const tail = collapsed ? breadcrumbs.slice(-2) : breadcrumbs.slice(head.length)
	const hiddenFrom = head.length
	const hiddenTo = breadcrumbs.length - tail.length

	const pathTo = (index: number) => breadcrumbs.slice(0, index + 1).join("/")

	const crumb = (name: string, index: number) => (
		<span className="crumbs-inner" key={`${name}-${index}`}>
			<button
				type="button"
				className="crumb"
				title={name}
				onClick={(e) => {
					e.stopPropagation()
					navigate(pathTo(index))
				}}
			>
				{name}
			</button>
			<button
				type="button"
				className="crumb-sep"
				aria-label={`${name} 的同级目录`}
				onClick={(e) => {
					e.stopPropagation()
					const r = e.currentTarget.getBoundingClientRect()
					setSiblingMenu({ x: r.left, y: r.bottom + 2, level: index })
				}}
			>
				›
			</button>
		</span>
	)

	return (
		<div className="crumbs" ref={boxRef} onClick={startEdit} title="点击编辑完整路径">
			<div className="crumbs-inner" ref={innerRef}>
				{/* 固定的根一级：点它回到 Bucket 列表，点它的 › 直接挑 Bucket */}
				<span className="crumbs-inner">
					<button
						type="button"
						className="crumb"
						onClick={(e) => {
							e.stopPropagation()
							navigate("")
						}}
					>
						oss://
					</button>
					<button
						type="button"
						className="crumb-sep"
						aria-label="选择 Bucket"
						onClick={(e) => {
							e.stopPropagation()
							const r = e.currentTarget.getBoundingClientRect()
							setSiblingMenu({ x: r.left, y: r.bottom + 2, level: 0 })
						}}
					>
						›
					</button>
				</span>
				{head.map((name, i) => crumb(name, i))}
				{collapsed && (
					<button
						type="button"
						className="crumb-ellipsis"
						aria-label="展开被折叠的层级"
						onClick={(e) => {
							e.stopPropagation()
							setExpanded(true)
						}}
						title={breadcrumbs.slice(hiddenFrom, hiddenTo).join(" › ")}
					>
						…›
					</button>
				)}
				{tail.map((name, i) => crumb(name, hiddenTo + i))}
			</div>

			{siblingMenu && (
				<SiblingMenu
					x={siblingMenu.x}
					y={siblingMenu.y}
					level={siblingMenu.level}
					onClose={() => setSiblingMenu(null)}
				/>
			)}
		</div>
	)
}

/* 某一级的同级目录 = 它父级下的 CommonPrefixes。
   第 0 级是 bucket，同级就是账号下所有 bucket。 */
function SiblingMenu({
	x,
	y,
	level,
	onClose,
}: {
	x: number
	y: number
	level: number
	onClose: () => void
}) {
	const breadcrumbs = useExplorerStore((s) => s.breadcrumbs)
	const navigate = useExplorerStore((s) => s.navigate)
	const buckets = useSessionStore((s) => s.buckets)
	const auth = useSessionStore((s) => s.auth)
	const appendLog = useSessionStore((s) => s.appendLog)

	const [items, setItems] = useState<MenuItem[] | null>(null)

	useEffect(() => {
		let cancelled = false

		const load = async () => {
			if (level === 0) {
				return buckets.map<MenuItem>((b) => ({
					label: b.name,
					checked: b.name === breadcrumbs[0],
					checkable: true,
					onSelect: () => navigate(b.name),
				}))
			}
			const parentParts = breadcrumbs.slice(0, level)
			const { bucket, prefix } = splitPath(parentParts.join("/"))
			const dir = prefix ? `${prefix}/` : ""
			const args = ["--bucket", bucket, "--delimiter", "/", "--max-keys", "1000"]
			if (dir) args.push("--prefix", dir)
			const json = await ossApi(auth(), "list-objects-v2", args)
			return pickObjects(json, dir)
				.filter((o) => o.folder)
				.map<MenuItem>((o) => ({
					label: o.name,
					checked: o.name === breadcrumbs[level],
					checkable: true,
					onSelect: () => navigate(joinPath(bucket, `${prefix ? prefix + "/" : ""}${o.name}`)),
				}))
		}

		load()
			.then((result) => {
				if (!cancelled) setItems(result.length ? result : [{ label: "（没有同级目录）", disabled: true }])
			})
			.catch((err) => {
				appendLog(`[crumb] ${err}`)
				if (!cancelled) setItems([{ label: friendlyError(err), disabled: true }])
			})

		return () => {
			cancelled = true
		}
	}, [level, breadcrumbs, buckets, auth, navigate, appendLog])

	return <Menu x={x} y={y} items={items ?? [{ label: "读取中…", disabled: true }]} onClose={onClose} />
}
