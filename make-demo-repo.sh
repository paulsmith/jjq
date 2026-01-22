#!/bin/bash

set -euo pipefail

dest_dir=

if [ $# -lt 1 ]; then
    dest_dir=$(mktemp -d)
    trap "rm -rf $dest_dir" EXIT
else
    dest_dir="$1"
fi

jj git init "$dest_dir"
pushd "$dest_dir"
go mod init example/jjdemo
go mod edit -go=1.24
cat > main.go <<EOF
package main
import "fmt"
func main() {
    fmt.Println("Hello, world!")
}
EOF
cat > main_test.go <<EOF
package main_test
import (
    "testing"
    "os/exec"
)
func TestMain(t *testing.T) {
    cmd := exec.Command("go", "run", ".")
    out, err := cmd.CombinedOutput()
    if err != nil {
        t.Fatal(err)
    }
    want := "Hello, world!\n" 
    if string(out) != want {
        t.Errorf("want %q, got %q", want, string(out))
    }
}
EOF
go fmt ./...
cat > Makefile <<EOF
all: test
test:
	go test -v ./...
	golangci-lint run
EOF
jj desc -m "initial"
jj bookmark c main

jj new -m "add greeting pkg"
mkdir say
cat > say/greet.go <<EOF
package say

func Greet(name string) string {
    return "Hello, " + name + "!"
}
EOF
sed -i '' $'/import "fmt"/a\\\nimport "example\/jjdemo/say"' main.go
sed -i '' 's/fmt\.Println\(.*\)/fmt.Println(say.Greet("world"))/' main.go
go fmt ./...
jj bookmark c pr1
jj log -T '' --no-graph

jj new -m "add goodbye" main
mkdir say
cat > say/bye.go <<EOF
package say

func Bye() string {
    return "Goodbye."
}
EOF
sed -i '' $'/import "fmt"/a\\\nimport "example\/jjdemo/say"' main.go
sed -i '' $'/fmt\.Println/a\\\n\tfmt.Println(say.Bye())' main.go
sed -i '' 's/"Hello, world!\\n"/"Hello, world!\\nGoodbye.\\n"/' main_test.go
go fmt ./...
jj bookmark c pr2
jj log -T '' --no-graph

jj new -m "add comment" main
sed -i '' $'/func main/a\\\n\/\/ say hi\\\n' main.go
go fmt ./...
jj bookmark c pr3
jj log -T '' --no-graph

jj new -m "add readme" main
echo '# jjq demo' > README.md
jj bookmark c pr4
jj log -T '' --no-graph

jj new main

popd
