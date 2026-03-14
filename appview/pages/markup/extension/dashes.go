package extension

import (
	"github.com/yuin/goldmark"
	gast "github.com/yuin/goldmark/ast"
	"github.com/yuin/goldmark/parser"
	"github.com/yuin/goldmark/text"
	"github.com/yuin/goldmark/util"
)

type dashParser struct{}

func (p *dashParser) Trigger() []byte {
	return []byte{'-'}
}

func (p *dashParser) Parse(parent gast.Node, block text.Reader, pc parser.Context) gast.Node {
	line, _ := block.PeekLine()
	if len(line) < 2 || line[0] != '-' || line[1] != '-' {
		return nil
	}
	node := gast.NewString([]byte("\u2014"))
	node.SetCode(true)
	block.Advance(2)
	return node
}

type digitDashParser struct{}

func (p *digitDashParser) Trigger() []byte {
	return []byte{'-'}
}

func (p *digitDashParser) Parse(parent gast.Node, block text.Reader, pc parser.Context) gast.Node {
	line, _ := block.PeekLine()
	if len(line) < 2 {
		return nil
	}
	before := block.PrecendingCharacter()
	if before < '0' || before > '9' {
		return nil
	}
	if line[1] < '0' || line[1] > '9' {
		return nil
	}
	node := gast.NewString([]byte("\u2013"))
	node.SetCode(true)
	block.Advance(1)
	return node
}

type dashExt struct{}

// Dashes replaces "--" with an em-dash (—) and a hyphen between two digits
// with an en-dash (–). Implemented as an inline parser so it operates on the
// raw byte stream, unaffected by hard-wrapped source lines.
var Dashes goldmark.Extender = &dashExt{}

func (e *dashExt) Extend(m goldmark.Markdown) {
	m.Parser().AddOptions(parser.WithInlineParsers(
		util.Prioritized(&dashParser{}, 9990),
		util.Prioritized(&digitDashParser{}, 9991),
	))
}
