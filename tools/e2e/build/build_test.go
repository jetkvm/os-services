package build

import (
	"archive/tar"
	"bytes"
	"context"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"testing"
)

func TestArchiveHasTheWorkingTreeAsOnDisk(t *testing.T) {
	root := t.TempDir()
	git := func(args ...string) {
		command := exec.Command("git", append([]string{"-C", root, "-c", "user.name=t", "-c", "user.email=t@t"}, args...)...)
		if output, err := command.CombinedOutput(); err != nil {
			t.Fatalf("git %v: %v: %s", args, err, output)
		}
	}
	write := func(name, content string, mode os.FileMode) {
		path := filepath.Join(root, name)
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(content), mode); err != nil {
			t.Fatal(err)
		}
	}
	git("init", "-q")
	write(".gitignore", "target/\n", 0o644)
	write("src/main.rs", "committed", 0o644)
	write("run.sh", "#!/bin/sh\n", 0o755)
	write("deleted.txt", "gone", 0o644)
	git("add", ".")
	git("commit", "-qm", "tree")
	write("src/main.rs", "changed", 0o644)
	write("new file.txt", "not committed", 0o644)
	write("target/big", "ignored", 0o644)
	if err := os.Remove(filepath.Join(root, "deleted.txt")); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("src/main.rs", filepath.Join(root, "link")); err != nil {
		t.Fatal(err)
	}

	tree, err := archive(context.Background(), root)
	if err != nil {
		t.Fatal(err)
	}
	got := map[string]string{}
	reader := tar.NewReader(bytes.NewReader(tree))
	for {
		header, err := reader.Next()
		if err == io.EOF {
			break
		}
		if err != nil {
			t.Fatal(err)
		}
		content, _ := io.ReadAll(reader)
		got[header.Name] = string(content)
		switch header.Name {
		case "run.sh":
			if header.Mode&0o100 == 0 {
				t.Errorf("run.sh lost its executable bit: %o", header.Mode)
			}
		case "link":
			if header.Typeflag != tar.TypeSymlink || header.Linkname != "src/main.rs" {
				t.Errorf("link: type %c to %q, want a symbolic link to src/main.rs", header.Typeflag, header.Linkname)
			}
		}
	}
	want := map[string]string{
		".gitignore":   "target/\n",
		"src/main.rs":  "changed",
		"run.sh":       "#!/bin/sh\n",
		"new file.txt": "not committed",
		"link":         "",
	}
	if len(got) != len(want) {
		t.Errorf("got files %v, want %v", got, want)
	}
	for name, content := range want {
		if got[name] != content {
			t.Errorf("%s: got %q, want %q", name, got[name], content)
		}
	}
}

func TestRemotePathIsOneWordAndKeepsHome(t *testing.T) {
	home := t.TempDir()
	for path, want := range map[string]string{
		"~/build/os services": filepath.Join(home, "build/os services"),
		"/c/Test User/build":  "/c/Test User/build",
		"it's":                "it's",
	} {
		command := exec.Command("sh", "-c", `printf %s `+remotePath(path))
		command.Env = append(os.Environ(), "HOME="+home)
		output, err := command.Output()
		if err != nil {
			t.Fatal(err)
		}
		if string(output) != want {
			t.Errorf("%s: got %q, want %q", path, output, want)
		}
	}
}

func TestCheckDirRefusesHomeFoldersAndRoots(t *testing.T) {
	for _, dir := range []string{"", ".", "./", "~", "~/", "~/.", "/", "//", "C:", "C:/", `C:\`, "~/build/..", "~/..", "/srv/../home/user", `C:\build\..`} {
		if checkDir(dir) == nil {
			t.Errorf("%q: accepted", dir)
		}
	}
	for _, dir := range []string{"~/build/os-services", "C:/build/os-services", `C:\build\os-services`, "/srv/build", "build/os-services", "~/..build"} {
		if err := checkDir(dir); err != nil {
			t.Errorf("%q: %v", dir, err)
		}
	}
}

// receive runs receiveScript for dir with sh, as an SSH session of a user
// with the home folder home does, and with a tree of one file on stdin.
func receive(t *testing.T, home, dir string) error {
	t.Helper()
	var tree bytes.Buffer
	writer := tar.NewWriter(&tree)
	if err := writer.WriteHeader(&tar.Header{Name: "Cargo.toml", Mode: 0o644, Size: 3}); err != nil {
		t.Fatal(err)
	}
	if _, err := writer.Write([]byte("new")); err != nil {
		t.Fatal(err)
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	command := exec.Command("sh", "-c", receiveScript(remotePath(dir)))
	command.Dir = home
	command.Env = append(os.Environ(), "HOME="+home)
	command.Stdin = &tree
	output, err := command.CombinedOutput()
	t.Logf("%s: %v: %s", dir, err, output)
	return err
}

// names are the entries of dir, sorted.
func names(t *testing.T, dir string) []string {
	t.Helper()
	entries, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	var names []string
	for _, entry := range entries {
		names = append(names, entry.Name())
	}
	return names
}

func TestReceiveScriptDeletesOnlyInABuildFolder(t *testing.T) {
	// A home folder with a file, a folder that is not a build folder, and
	// a link to the home folder.
	newHome := func(t *testing.T) string {
		home := filepath.Join(t.TempDir(), "home", "user")
		for _, file := range []string{"notes.txt", "documents/letter.txt"} {
			if err := os.MkdirAll(filepath.Dir(filepath.Join(home, file)), 0o755); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(home, file), []byte("keep"), 0o644); err != nil {
				t.Fatal(err)
			}
		}
		if err := os.Symlink(home, filepath.Join(home, "documents", "link-to-home")); err != nil {
			t.Fatal(err)
		}
		return home
	}

	for _, dir := range []string{"~/", ".", "~/documents/..", "HOME", "PARENT", "~/documents/link-to-home", "~/documents"} {
		home := newHome(t)
		before := names(t, home)
		switch dir {
		case "HOME":
			dir = home
		case "PARENT":
			dir = filepath.Dir(home)
		}
		if receive(t, home, dir) == nil {
			t.Errorf("%s: the script cleaned a folder that is not a build folder", dir)
		}
		if after := names(t, home); !slices.Equal(after, before) {
			t.Errorf("%s: the home folder has %v, want %v", dir, after, before)
		}
		if after := names(t, filepath.Join(home, "documents")); !slices.Equal(after, []string{"letter.txt", "link-to-home"}) {
			t.Errorf("%s: documents has %v", dir, after)
		}
	}

	// A home folder without files is still not a build folder.
	empty := t.TempDir()
	if receive(t, empty, "~/") == nil {
		t.Error("the script made an empty home folder a build folder")
	}
	if after := names(t, empty); len(after) != 0 {
		t.Errorf("the empty home folder has %v", after)
	}

	// A new folder becomes a build folder. The next build replaces its
	// tree and keeps target/.
	home := newHome(t)
	build := filepath.Join(home, "build", "os-services")
	if err := receive(t, home, "~/build/os-services"); err != nil {
		t.Fatalf("a new folder: %v", err)
	}
	if err := os.MkdirAll(filepath.Join(build, "target"), 0o755); err != nil {
		t.Fatal(err)
	}
	for _, file := range []string{"target/cache", "stale.rs", "Cargo.toml"} {
		if err := os.WriteFile(filepath.Join(build, file), []byte("old"), 0o644); err != nil {
			t.Fatal(err)
		}
	}
	if err := receive(t, home, "~/build/os-services"); err != nil {
		t.Fatalf("a build folder: %v", err)
	}
	if after := names(t, build); !slices.Equal(after, []string{marker, "Cargo.toml", "target"}) {
		t.Errorf("the build folder has %v", after)
	}
	if content, _ := os.ReadFile(filepath.Join(build, "Cargo.toml")); string(content) != "new" {
		t.Errorf("Cargo.toml: got %q, want the sent file", content)
	}
	if content, _ := os.ReadFile(filepath.Join(build, "target", "cache")); string(content) != "old" {
		t.Errorf("target/cache: got %q, want it kept", content)
	}
}
