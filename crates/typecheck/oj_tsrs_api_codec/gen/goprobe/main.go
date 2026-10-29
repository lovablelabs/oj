// Pinned-source probe (copied into ts-ref/tsc/cmd/codecprobe by gen/goprobe.sh, built with the pinned module,
// then removed).
//
//	codecprobe getindex <fixtures dir>
//	  parses and binds every fixture exactly like the API server's parse cache (snapshothost.AcquireSourceFile),
//	  builds encoder.GetNodeIndexTable and prints `<fixture> <index> <GetIndex(node at index)>` for every node
//	  record: which index Go picks for nodes that occur more than once.
//	codecprobe reencode <out dir> <encoded file>...
//	  encoder.DecodeNodes then encoder.EncodeNode(decoded, nil) (EncodeSourceFile for a SourceFile root) for each file, written to
//	  <out dir>/<base>.goreencoded.bin, or the recovered panic/error to <base>.goreencoded.err.
package main

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/microsoft/TypeScript/tsc/internal/api/encoder"
	"github.com/microsoft/TypeScript/tsc/internal/ast"
	"github.com/microsoft/TypeScript/tsc/internal/binder"
	"github.com/microsoft/TypeScript/tsc/internal/core"
	"github.com/microsoft/TypeScript/tsc/internal/parser"
	"github.com/microsoft/TypeScript/tsc/internal/tspath"
	"github.com/zeebo/xxh3"
)

func main() {
	switch os.Args[1] {
	case "getindex":
		getIndex(os.Args[2])
	case "reencode":
		for _, f := range os.Args[3:] {
			reencode(os.Args[2], f)
		}
	default:
		panic("usage")
	}
}

func reencode(outDir string, file string) {
	data, err := os.ReadFile(file)
	if err != nil {
		panic(err)
	}
	base := filepath.Join(outDir, strings.TrimSuffix(filepath.Base(file), ".bin")+".goreencoded")
	out, err := func() (out []byte, err error) {
		defer func() {
			if r := recover(); r != nil {
				err = fmt.Errorf("panic: %v", r)
			}
		}()
		node, err := encoder.DecodeNodes(data)
		if err != nil {
			return nil, err
		}
		if node.Kind == ast.KindSourceFile {
			// EncodeNode(sourceFile, nil) dereferences the nil file; a decoded file is its own source file.
			out, _, err = encoder.EncodeSourceFile(node.AsSourceFile())
		} else {
			out, _, err = encoder.EncodeNode(node, nil)
		}
		return out, err
	}()
	_ = os.Remove(base + ".bin")
	_ = os.Remove(base + ".err")
	if err != nil {
		if err := os.WriteFile(base+".err", []byte(err.Error()+"\n"), 0o644); err != nil {
			panic(err)
		}
		return
	}
	if err := os.WriteFile(base+".bin", out, 0o644); err != nil {
		panic(err)
	}
}

func getIndex(dir string) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		panic(err)
	}
	names := []string{}
	for _, e := range entries {
		if e.Name()[0] != '.' {
			names = append(names, e.Name())
		}
	}
	sort.Strings(names)
	for _, name := range names {
		text, err := os.ReadFile(filepath.Join(dir, name))
		if err != nil {
			panic(err)
		}
		fileName := "/fixtures/" + name
		file := parser.ParseSourceFile(ast.SourceFileParseOptions{FileName: fileName, Path: tspath.Path(fileName)}, string(text), core.EnsureScriptKindFromFileName(fileName))
		file.Hash = xxh3.HashString128(string(text))
		binder.BindSourceFile(file)
		table := encoder.GetNodeIndexTable(file)
		for i, node := range table.Nodes {
			if node != nil {
				fmt.Printf("%s %d %d\n", name, i, table.GetIndex(node))
			}
		}
	}
}
