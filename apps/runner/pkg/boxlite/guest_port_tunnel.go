package boxlite

import (
	"context"
	"errors"
	"fmt"
	"net"

	boxlite "github.com/boxlite-ai/boxlite/sdks/go"
)

// ErrBoxNotRunning is returned by DialGuestPort for a box that is not up.
var ErrBoxNotRunning = errors.New("box is not running")

// DialGuestPort opens a raw TCP stream to a port inside a running box.
//
// It refuses a box that is not running instead of booting it. A fresh handle
// boots its box on first use, so a dial used to start a stopped box behind the
// control plane's back: the box served traffic while the API still recorded it
// as stopped. The preview proxy answers ErrBoxNotRunning by asking the API to
// start the box, which keeps the API the only path that wakes one.
func (c *Client) DialGuestPort(ctx context.Context, boxID string, port uint16) (net.Conn, error) {
	// Read the persisted state through the runtime: it never boots the box.
	info, err := c.runtime.GetInfo(ctx, boxID)
	if err != nil {
		return nil, fmt.Errorf("read state of box %s: %w", boxID, err)
	}
	if info.State != boxlite.StateRunning && info.State != boxlite.StatePaused {
		return nil, fmt.Errorf("%w: %s is %s", ErrBoxNotRunning, boxID, info.State)
	}

	bx, err := c.getOrFetchBox(ctx, boxID)
	if err != nil {
		return nil, err
	}

	network, err := bx.Network()
	if err != nil {
		return nil, fmt.Errorf("open box network handle for %s: %w", boxID, err)
	}
	defer network.Close()

	tunnel, err := network.Tunnel(ctx, port)
	if err != nil {
		return nil, fmt.Errorf("prepare guest TCP tunnel to %s port %d: %w", boxID, port, err)
	}

	conn, err := tunnel.Connect(ctx)
	if err != nil {
		return nil, fmt.Errorf("connect guest TCP tunnel to %s port %d: %w", boxID, port, err)
	}
	return conn, nil
}
