package web

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

const csrfSecretBytes = 32

type csrfSigner struct {
	secret []byte
}

func loadCSRFSigner(path string) (*csrfSigner, error) {
	secret, err := os.ReadFile(path)
	if err == nil {
		if len(secret) != csrfSecretBytes {
			return nil, fmt.Errorf("CSRF secret %s must be %d bytes", path, csrfSecretBytes)
		}
		return &csrfSigner{secret: secret}, nil
	}
	if !errors.Is(err, os.ErrNotExist) {
		return nil, fmt.Errorf("read CSRF secret: %w", err)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return nil, fmt.Errorf("create CSRF secret directory: %w", err)
	}
	secret = make([]byte, csrfSecretBytes)
	if _, err := rand.Read(secret); err != nil {
		return nil, fmt.Errorf("generate CSRF secret: %w", err)
	}
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return nil, fmt.Errorf("create CSRF secret: %w", err)
	}
	if _, err := file.Write(secret); err != nil {
		file.Close()
		return nil, fmt.Errorf("write CSRF secret: %w", err)
	}
	if err := file.Close(); err != nil {
		return nil, fmt.Errorf("close CSRF secret: %w", err)
	}
	return &csrfSigner{secret: secret}, nil
}

func (signer *csrfSigner) token(values ...string) string {
	digest := hmac.New(sha256.New, signer.secret)
	for _, value := range values {
		digest.Write([]byte{0})
		digest.Write([]byte(value))
	}
	return base64.RawURLEncoding.EncodeToString(digest.Sum(nil))
}

func (signer *csrfSigner) valid(token string, values ...string) bool {
	provided, err := base64.RawURLEncoding.DecodeString(token)
	if err != nil {
		return false
	}
	expected, err := base64.RawURLEncoding.DecodeString(signer.token(values...))
	if err != nil {
		return false
	}
	return hmac.Equal(provided, expected)
}
