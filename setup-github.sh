#!/bin/bash

# GitHub Repository 設定腳本
# 使用方法: ./setup-github.sh YOUR_USERNAME

if [ -z "$1" ]; then
    echo "錯誤: 請提供您的 GitHub 用戶名"
    echo "使用方法: ./setup-github.sh YOUR_USERNAME"
    exit 1
fi

USERNAME=$1
REPO_NAME="intelligent-dam"

echo "正在設定 GitHub repository..."
echo "用戶名: $USERNAME"
echo "Repository 名稱: $REPO_NAME"

# 檢查是否已有遠端倉庫
if git remote get-url origin &>/dev/null; then
    echo "警告: 已存在遠端倉庫 'origin'"
    read -p "是否要更新遠端 URL? (y/n) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        git remote set-url origin "https://github.com/$USERNAME/$REPO_NAME.git"
    else
        echo "跳過遠端 URL 設定"
    fi
else
    echo "添加遠端倉庫..."
    git remote add origin "https://github.com/$USERNAME/$REPO_NAME.git"
fi

echo ""
echo "=========================================="
echo "下一步:"
echo "1. 前往 https://github.com/new 創建新的 repository"
echo "2. Repository 名稱: $REPO_NAME"
echo "3. Description: Intelligent Digital Asset Management System"
echo "4. 選擇 Public 或 Private"
echo "5. **不要**勾選 'Initialize this repository with a README'"
echo "6. 點擊 'Create repository'"
echo ""
echo "創建完成後，執行以下命令推送代碼:"
echo "  git push -u origin main"
echo "=========================================="

