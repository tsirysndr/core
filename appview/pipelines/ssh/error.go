package ssh

import (
	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
)

var (
	colorBrightRed lipgloss.ANSIColor = 9
)

type errorModel struct {
	renderer *lipgloss.Renderer
	message  string
}

func newErrorModel(renderer *lipgloss.Renderer, message string) *errorModel {
	return &errorModel{renderer: renderer, message: message}
}

func (m *errorModel) Init() tea.Cmd {
	return tea.Quit
}

func (m *errorModel) Update(_ tea.Msg) (tea.Model, tea.Cmd) {
	return m, tea.Quit
}

func (m *errorModel) View() string {
	return m.renderer.NewStyle().Foreground(colorBrightRed).Render("error: "+m.message) + "\n"
}
