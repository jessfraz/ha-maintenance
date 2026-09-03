package web

import (
	"os"
	"path/filepath"
	"testing"
)

func TestCSRFSignerPersistsAndBindsToken(t *testing.T) {
	t.Parallel()

	path := filepath.Join(t.TempDir(), "csrf.key")
	first, err := loadCSRFSigner(path)
	if err != nil {
		t.Fatalf("loadCSRFSigner() create error = %v", err)
	}
	token := first.token("user-1", "example-filter", "todo-1")
	if !first.valid(token, "user-1", "example-filter", "todo-1") {
		t.Fatal("created signer rejected its token")
	}
	if first.valid(token, "user-2", "example-filter", "todo-1") {
		t.Fatal("token was valid for a different Home Assistant user")
	}
	if first.valid(token, "user-1", "example-filter", "todo-2") {
		t.Fatal("token was valid for a different todo occurrence")
	}

	second, err := loadCSRFSigner(path)
	if err != nil {
		t.Fatalf("loadCSRFSigner() reopen error = %v", err)
	}
	if !second.valid(token, "user-1", "example-filter", "todo-1") {
		t.Fatal("reopened signer rejected persisted token")
	}
	info, err := os.Stat(path)
	if err != nil {
		t.Fatalf("Stat() error = %v", err)
	}
	if permissions := info.Mode().Perm(); permissions != 0o600 {
		t.Fatalf("CSRF secret permissions = %o, want 600", permissions)
	}
}
