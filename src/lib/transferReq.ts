/* 上传和下载共用同一个 UploadRequest 形状（后端就是一个 struct），
   凭证字段一个都不能少，所以集中构造一次，别在两处各抄一遍。 */

import type { Config } from "../store/session.ts"

export const newTaskId = () =>
	globalThis.crypto?.randomUUID?.() ?? `t-${Date.now()}-${Math.random().toString(36).slice(2)}`

export function uploadReqFor(
	config: Config,
	localPath: string,
	bucket: string,
	prefix: string,
): Record<string, unknown> {
	return {
		localPath,
		bucket,
		prefix,
		accessKeyId: config.accessKeyId.trim(),
		accessKeySecret: config.accessKeySecret,
		endpoint: config.endpoint,
		jobs: config.jobs,
		parallel: config.parallel,
		partSizeMb: config.partSizeMb,
		ossutilPath: config.ossutilPath.trim(),
		cliCreds: config.cliCreds,
		direct: config.direct,
	}
}
