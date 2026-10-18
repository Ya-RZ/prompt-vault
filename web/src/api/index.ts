/**
 * API 层唯一出口：所有页面只从这里 import。
 * 类型定义与 INTERFACES.md 字段一一对应（见 ./types.ts）。
 */

export * from './types';
export { ApiError, apiRequest, describeError, API_PREFIX, toQueryString } from './client';
export type { ErrorPresentation, QueryValue, RequestOptions } from './client';

export { fetchHealth } from './health';
export {
  createAssetPrompt,
  deleteAsset,
  getAsset,
  getSimilarAssets,
  listAssets,
  updateAssetMeta,
  uploadAsset,
} from './assets';
export { cleanupTags, deleteTag, listTags } from './tags';
export {
  getSimilarPrompts,
  searchImageByAssetId,
  searchImageByBase64,
  searchImageByFile,
  searchLexical,
  searchText,
} from './search';
export {
  cancelJob,
  createEncodeJob,
  createImportJob,
  deleteJob,
  getJob,
  listJobErrors,
  listJobs,
  pauseJob,
  resumeJob,
} from './jobs';
export type { JobListQuery } from './jobs';
export { fetchFacets, fetchSize, fetchStats } from './stats';
