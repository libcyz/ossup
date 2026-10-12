

import { create } from "zustand"
import { persist } from "zustand/middleware"

import type { ViewMode } from "./explorer.ts"

export type Favorite = {
	/** 就是路径本身，形如 bucket/a/b —— 同一路径不会收藏两次 */
	id: string
	name: string
	path: string
}


export const DEFAULT_COLUMNS = [22, 12, 9, 7]

type PrefsState = {
	favorites: Favorite[]
	sidebarWidthRem: number
	columnWidthsRem: number[]
	sidebarCollapsed: boolean
	viewMode: ViewMode

	addFavorite: (path: string) => void
	removeFavorite: (id: string) => void
	removeOthers: (id: string) => void
	renameFavorite: (id: string, name: string) => void
	reorderFavorites: (from: number, to: number) => void
	isFavorite: (path: string) => boolean
	setSidebarWidth: (rem: number) => void
	setColumnWidth: (index: number, rem: number) => void
	toggleSidebar: () => void
	setViewMode: (mode: ViewMode) => void
}

/* bucket 根显示 bucket 名，深层目录显示最后一段 */
const labelOf = (path: string) => path.split("/").filter(Boolean).pop() ?? path

export const usePrefsStore = create<PrefsState>()(
	persist(
		(set, get) => ({
			favorites: [],
			sidebarWidthRem: 15,
			columnWidthsRem: DEFAULT_COLUMNS,
			sidebarCollapsed: false,
			viewMode: "details",

			addFavorite: (path) => {
				const clean = path.replace(/^\/+|\/+$/g, "")
				if (!clean || get().favorites.some((f) => f.path === clean)) return
				set((s) => ({
					favorites: [...s.favorites, { id: clean, name: labelOf(clean), path: clean }],
				}))
			},

			removeFavorite: (id) =>
				set((s) => ({ favorites: s.favorites.filter((f) => f.id !== id) })),

			removeOthers: (id) =>
				set((s) => ({ favorites: s.favorites.filter((f) => f.id === id) })),

			renameFavorite: (id, name) =>
				set((s) => ({
					favorites: s.favorites.map((f) =>
						/* 名字留空就退回路径末段，不要一个空白条目 */
						f.id === id ? { ...f, name: name.trim() || labelOf(f.path) } : f,
					),
				})),

			reorderFavorites: (from, to) =>
				set((s) => {
					if (from === to || from < 0 || from >= s.favorites.length) return s
					const favorites = [...s.favorites]
					const [moved] = favorites.splice(from, 1)
					favorites.splice(Math.max(0, Math.min(favorites.length, to)), 0, moved)
					return { favorites }
				}),

			isFavorite: (path) => get().favorites.some((f) => f.path === path),

			setSidebarWidth: (rem) => set({ sidebarWidthRem: rem }),

			setColumnWidth: (index, rem) =>
				set((s) => {
					const columnWidthsRem = [...s.columnWidthsRem]
					columnWidthsRem[index] = rem
					return { columnWidthsRem }
				}),

			toggleSidebar: () => set((s) => ({ sidebarCollapsed: !s.sidebarCollapsed })),

			setViewMode: (viewMode) => set({ viewMode }),
		}),
		{ name: "ossup-ui" },
	),
)
