package filetree

import (
	"path/filepath"
	"sort"
	"strings"
)

type FileTreeNode struct {
	Name        string
	Path        string
	IsDirectory bool
	Level       int
	Children    map[string]*FileTreeNode
}

// newNode creates a new node
func newNode(name, path string, isDir bool, level int) *FileTreeNode {
	return &FileTreeNode{
		Name:        name,
		Path:        path,
		IsDirectory: isDir,
		Level:       level,
		Children:    make(map[string]*FileTreeNode),
	}
}

func FileTree(files []string) *FileTreeNode {
	rootNode := newNode("", "", true, 0)

	sort.Strings(files)

	for _, file := range files {
		if file == "" {
			continue
		}

		parts := strings.Split(filepath.Clean(file), "/")
		if len(parts) == 0 {
			continue
		}

		currentNode := rootNode
		currentPath := ""

		for i, part := range parts {
			if currentPath == "" {
				currentPath = part
			} else {
				currentPath = filepath.Join(currentPath, part)
			}

			isDir := i < len(parts)-1
			level := i + 1

			if _, exists := currentNode.Children[part]; !exists {
				currentNode.Children[part] = newNode(part, currentPath, isDir, level)
			}

			currentNode = currentNode.Children[part]
		}
	}

	return rootNode
}
