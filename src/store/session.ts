/* 凭证 / endpoint / 连接态 / Bucket 列表。连接门和所有 oss_api 调用都读它。 */

import { invoke } from "@tauri-apps/api/core"
import { create } from "zustand"

import { endpointOf, nextMarker, pickBuckets, type Bucket } from "../lib/buckets.ts"
import { ossApi, type Auth } from "../lib/ossApi.ts"

export type Account = {
	name: string
	accessKeyId: string
	accessKeySecret: string
	endpoint: string
}

export type Config = {
	accessKeyId: string
	accessKeySecret: string
	endpoint: string
	bucket: string
	prefix: string
	remember: boolean
	jobs: number
	parallel: number
	partSizeMb: number
	ossutilPath: string
	cliCreds: boolean
	/** 同时进行的传输任务上限 */
	maxTasks: number
	/** 绕过代理 / TUN 直连 OSS */
	direct: boolean
	accounts: Account[]
	buckets: string[]
}

const DEFAULTS: Config = {
	accessKeyId: "",
	accessKeySecret: "",
	endpoint: "",
	bucket: "",
	prefix: "",
	remember: false,
	jobs: 5,
	parallel: 8,
	partSizeMb: 16,
	ossutilPath: "",
	cliCreds: false,
	maxTasks: 3,
	direct: true,
	accounts: [],
	buckets: [],
}

/* 控制台复制来的常带 https:// 和结尾斜杠，ossutil 不认，统一擦掉 */
export const normalizeEndpoint = (v: string) =>
	v.trim().replace(/^https?:\/\//i, "").replace(/\/+$/, "")

type SessionState = {
	config: Config
	connected: boolean
	connecting: boolean
	buckets: Bucket[]
	engine: { state: "checking" | "ok" | "error"; text: string }
	logs: string[]

	setConfig: (patch: Partial<Config>) => void
	appendLog: (line: string) => void
	auth: () => Auth
	loadConfig: () => Promise<void>
	saveConfig: () => Promise<void>
	checkEngine: () => Promise<void>
	connect: () => Promise<void>
	disconnect: () => void
	loadBuckets: () => Promise<Bucket[]>
	/** 跨地域的 Bucket 必须换 endpoint，否则连不上 */
	useBucketEndpoint: (bucket: Bucket) => void

	/** 把当前凭证存成一个可切换的账号（同 AK 覆盖，不重复添加） */
	saveCurrentAsAccount: (name: string) => Promise<void>
	removeAccount: (accessKeyId: string) => Promise<void>
	/** 切到另一套凭证并重新连接 */
	switchAccount: (account: Account) => Promise<void>
}

export const useSessionStore = create<SessionState>((set, get) => ({
	config: DEFAULTS,
	connected: false,
	connecting: false,
	buckets: [],
	engine: { state: "checking", text: "正在检测 ossutil…" },
	logs: [],

	setConfig: (patch) => set((s) => ({ config: { ...s.config, ...patch } })),

	appendLog: (line) =>
		set((s) => ({ logs: [...s.logs, line].slice(-2000) })),

	auth: () => {
		const c = get().config
		return {
			accessKeyId: c.accessKeyId.trim(),
			accessKeySecret: c.accessKeySecret,
			endpoint: normalizeEndpoint(c.endpoint),
			ossutilPath: c.ossutilPath.trim(),
			direct: c.direct,
		}
	},

	loadConfig: async () => {
		try {
			const cfg = await invoke<Config>("load_config")
			set({ config: { ...DEFAULTS, ...cfg, endpoint: normalizeEndpoint(cfg.endpoint ?? "") } })
		} catch (err) {
			get().appendLog(`[config] 读取配置失败: ${err}`)
		}
	},

	saveConfig: async () => {
		const c = get().config
		try {
			await invoke("save_config", {
				cfg: {
					...c,
					accessKeyId: c.remember ? c.accessKeyId.trim() : "",
					accessKeySecret: c.remember ? c.accessKeySecret : "",
					endpoint: normalizeEndpoint(c.endpoint),
				},
			})
		} catch (err) {
			get().appendLog(`[config] 保存配置失败: ${err}`)
		}
	},

	checkEngine: async () => {
		set({ engine: { state: "checking", text: "正在检测 ossutil…" } })
		try {
			const version = await invoke<string>("check_ossutil", {
				ossutilPath: get().config.ossutilPath.trim(),
			})
			set({ engine: { state: "ok", text: version } })
		} catch (err) {
			set({ engine: { state: "error", text: "未找到 ossutil" } })
			get().appendLog(`[engine] ${err}`)
		}
	},

	loadBuckets: async () => {
		/* 一次最多 1000 个，Bucket 多的账号必须跟着 NextMarker 翻页。
		   页数封顶纯粹是防服务端一直说 truncated 死循环。 */
		const all: Bucket[] = []
		let marker = ""
		let last: any = null
		for (let page = 0; page < 50; page++) {
			const args = ["--max-keys", "1000"]
			if (marker) args.push("--marker", marker)
			last = await ossApi(get().auth(), "list-buckets", args)
			all.push(...pickBuckets(last))
			marker = nextMarker(last)
			if (!marker) break
		}
		if (!all.length && last) {
			get().appendLog(
				`[buckets] 没能从返回里认出 Bucket，原始 JSON:\n${JSON.stringify(last, null, 2)}`,
			)
		}
		set({ buckets: all })
		return all
	},

	/* 连接 = 真跑一次 list-buckets。既验证了凭证，又顺手把列表拿回来了。 */
	connect: async () => {
		set({ connecting: true })
		try {
			await get().loadBuckets()
			set({ connected: true })
			/* 连通过的凭证自动进账号列表，否则"切换账号"永远是个空菜单。
			   已在列表里的不动，免得把用户起的名字冲掉。 */
			const c = get().config
			if (!c.accounts.some((a) => a.accessKeyId === c.accessKeyId.trim())) {
				await get().saveCurrentAsAccount("")
			}
			await get().saveConfig()
		} finally {
			set({ connecting: false })
		}
	},

	disconnect: () => {
		const { remember } = get().config
		set((s) => ({
			connected: false,
			buckets: [],
			/* 没勾"记住凭证"就别把密钥留在内存和输入框里 */
			config: remember
				? s.config
				: { ...s.config, accessKeyId: "", accessKeySecret: "" },
		}))
	},

	saveCurrentAsAccount: async (name) => {
		const c = get().config
		if (!c.accessKeyId.trim() || !c.accessKeySecret) return
		const entry: Account = {
			name: name.trim() || c.accessKeyId.trim().slice(0, 8),
			accessKeyId: c.accessKeyId.trim(),
			accessKeySecret: c.accessKeySecret,
			endpoint: normalizeEndpoint(c.endpoint),
		}
		/* 同一个 AK 只留一条，改名或换 endpoint 就地更新 */
		const rest = c.accounts.filter((a) => a.accessKeyId !== entry.accessKeyId)
		get().setConfig({ accounts: [...rest, entry] })
		await get().saveConfig()
	},

	removeAccount: async (accessKeyId) => {
		get().setConfig({
			accounts: get().config.accounts.filter((a) => a.accessKeyId !== accessKeyId),
		})
		await get().saveConfig()
	},

	switchAccount: async (account) => {
		get().setConfig({
			accessKeyId: account.accessKeyId,
			accessKeySecret: account.accessKeySecret,
			endpoint: account.endpoint,
		})
		/* 换账号等于换了一整套可见资源，先断干净再连，
		   否则旧账号的 bucket 列表会残留一瞬间 */
		set({ connected: false, buckets: [] })
		await get().connect()
	},

	useBucketEndpoint: (bucket) => {
		const endpoint = endpointOf(bucket)
		if (endpoint && endpoint !== normalizeEndpoint(get().config.endpoint)) {
			get().setConfig({ endpoint })
		}
	},
}))
