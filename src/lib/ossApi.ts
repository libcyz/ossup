/* Rust 侧 oss_api 的前端封装。后端是 `ossutil api <op> [args] --output-format json`
   的泛用透传，所以这里也不给每个操作单独包一层函数。 */

import { invoke } from "@tauri-apps/api/core"

export type Auth = {
	accessKeyId: string
	accessKeySecret: string
	endpoint: string
	ossutilPath: string
	direct: boolean
}

export const ossApi = (auth: Auth, op: string, args: string[] = []): Promise<any> =>
	invoke("oss_api", { auth, op, args })

/* ossutil 把服务端错误整段抛上来（Error Code / Message / Request Id 好几行）。
   界面上只该显示一句人话，原文留给日志面板。 */
export function friendlyError(err: unknown): string {
	const raw = String(err)
	if (/AccessDenied|403/i.test(raw)) return "你没有权限查看此文件夹"
	if (/InvalidAccessKeyId/i.test(raw)) return "AccessKey ID 不存在"
	if (/SignatureDoesNotMatch/i.test(raw)) return "AccessKey Secret 不正确"
	if (/NoSuchBucket/i.test(raw)) return "Bucket 不存在"
	if (/timeout|timed out|dial tcp|no such host/i.test(raw)) return "连不上 OSS，检查网络和 Endpoint"
	if (/InvalidBucketName/i.test(raw)) return "Bucket 名或 Endpoint 地域不对"
	const first = raw.split("\n").find((l) => l.trim())
	return first?.replace(/^Error:\s*/, "").trim() || "读取失败"
}

/** 权限错误单独判，界面要显示锁图标而不是通用错误图标。 */
export const isPermissionError = (err: unknown) =>
	/AccessDenied|403/i.test(String(err))
