# GitHub Repository 設定指南

## 方法 1: 使用 GitHub CLI（推薦）

如果已安裝 GitHub CLI，執行以下命令：

```bash
# 1. 登入 GitHub（如果尚未登入）
gh auth login

# 2. 創建 repository 並推送
gh repo create intelligent-dam --public --source=. --remote=origin --push
```

## 方法 2: 手動創建（如果沒有 GitHub CLI）

### 步驟 1: 在 GitHub 網站創建 Repository

1. 前往 https://github.com/new
2. Repository name: `intelligent-dam` (或您喜歡的名稱)
3. Description: `Intelligent Digital Asset Management System - 智慧型本地影音資產管理系統`
4. 選擇 Public 或 Private
5. **不要**勾選 "Initialize this repository with a README"
6. 點擊 "Create repository"

### 步驟 2: 連接本地倉庫並推送

執行以下命令（將 `YOUR_USERNAME` 替換為您的 GitHub 用戶名）：

```bash
# 添加遠端倉庫
git remote add origin https://github.com/YOUR_USERNAME/intelligent-dam.git

# 將分支重命名為 main（如果 GitHub 使用 main 作為預設分支）
git branch -M main

# 推送代碼
git push -u origin main
```

如果您的 GitHub 使用 `master` 作為預設分支，則使用：

```bash
git push -u origin master
```

## 方法 3: 使用 SSH（如果您已設定 SSH 金鑰）

```bash
# 添加遠端倉庫（SSH）
git remote add origin git@github.com:YOUR_USERNAME/intelligent-dam.git

# 推送代碼
git branch -M main
git push -u origin main
```

## 驗證

推送完成後，前往 `https://github.com/YOUR_USERNAME/intelligent-dam` 確認所有文件都已上傳。

