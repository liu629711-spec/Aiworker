/**
 * secrets 业务的前端接口层：账密管理 (登录名/密码录入与查看，含独立条目)。
 */
import { invoke } from "@tauri-apps/api/core";
import type { SecretItem } from "../types";

export const secretsApi = {
  /** 全部条目 (每个已导入账号一行 + 独立录入条目) */
  list: () => invoke<SecretItem[]>("zcode_secrets_list"),
  /** 保存某条账密 (title 仅供独立条目改名) */
  save: (id: number, title: string, loginName: string, password: string) =>
    invoke<void>("zcode_secrets_save", { id, title, loginName, password }),
  /** 新增一条独立账密，返回新行 id */
  add: (title: string, loginName: string, password: string) =>
    invoke<number>("zcode_secrets_add", { title, loginName, password }),
  /** 删除独立账密条目 */
  remove: (id: number) => invoke<void>("zcode_secrets_delete", { id }),
  /** 打开账密管理弹窗 (已存在则置前) */
  openWindow: () => invoke<void>("zcode_secrets_open_window"),
  
  /** 批量导入 CSV 文件（格式：email,password）*/
  importFromCSV: (fileContent: string) => invoke<Record<string, any>>("zcode_accounts_import_from_csv", { fileContent }),
};
