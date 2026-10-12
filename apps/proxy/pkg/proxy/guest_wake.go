// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package proxy

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"time"

	common_errors "github.com/boxlite-ai/common-go/pkg/errors"
)

// A stopped box's first request should succeed when the box and its service
// come up within guestWakeBudget. Past that, the client gets 503 + Retry-After
// while the start continues in the background.
var guestWakeBudget = 10 * time.Second

const (
	guestDialRetryInterval = 200 * time.Millisecond
	boxStartingRetryAfter  = "3"
	runnerBoxNotRunning    = "box_not_running"
)

var (
	errBoxStarting    = common_errors.NewCustomError(http.StatusServiceUnavailable, "box is starting; retry shortly", "BOX_STARTING")
	errBoxStopped     = common_errors.NewCustomError(http.StatusServiceUnavailable, "box is stopped and auto-resume is disabled", "BOX_STOPPED")
	errBoxUnavailable = common_errors.NewCustomError(http.StatusConflict, "box cannot be started", "BOX_UNAVAILABLE")
)

// runnerTunnelError is a non-200 answer to the runner CONNECT.
type runnerTunnelError struct {
	statusCode int
	code       string
}

func (e *runnerTunnelError) Error() string {
	return fmt.Sprintf("runner CONNECT returned %d %s", e.statusCode, e.code)
}

// dialGuestPortWithWake dials a guest port and wakes a stopped box on the way.
//
// It dials first, so a running box costs no API call. The runner refuses a
// stopped box with box_not_running; that triggers one ensure-ready call to the
// API, which starts the box under the same rules as SDK auto-resume. A 502
// means the box is up but nothing listens on the port yet (a resumed box
// replays its cmd), so the dial is retried until the budget runs out.
func (p *Proxy) dialGuestPortWithWake(ctx context.Context, boxID string, port uint16) (net.Conn, error) {
	runnerInfo, err := p.getBoxRunnerInfo(ctx, boxID)
	if err != nil {
		return nil, fmt.Errorf("resolve runner for box %s: %w", boxID, err)
	}
	deadline := time.Now().Add(guestWakeBudget)
	woke := false
	for {
		conn, err := dialRunnerTunnel(ctx, runnerInfo, boxID, port)
		if err == nil {
			return conn, nil
		}
		var tunnelErr *runnerTunnelError
		if !errors.As(err, &tunnelErr) {
			return nil, err
		}
		switch {
		case tunnelErr.code == runnerBoxNotRunning && !woke:
			woke = true
			if err := p.wakeBox(ctx, boxID, deadline); err != nil {
				return nil, err
			}
		case tunnelErr.code == runnerBoxNotRunning, tunnelErr.statusCode == http.StatusBadGateway:
		default:
			return nil, err
		}
		if time.Until(deadline) < guestDialRetryInterval {
			return nil, errBoxStarting
		}
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		case <-time.After(guestDialRetryInterval):
		}
	}
}

// wakeBox asks the API to start the box. Concurrent requests for one box share
// a call; the API's state-change lock dedupes across proxy replicas. Only an
// answer that retrying cannot change is returned; anything else is logged and
// the caller keeps polling the dial.
func (p *Proxy) wakeBox(ctx context.Context, boxID string, deadline time.Time) error {
	result := p.wakeGroup.DoChan(boxID, func() (any, error) {
		// Detached so one client hanging up does not fail the others.
		wakeCtx, cancel := context.WithDeadline(context.WithoutCancel(ctx), deadline)
		defer cancel()
		return nil, p.requestEnsureReady(wakeCtx, boxID)
	})
	var err error
	select {
	case <-ctx.Done():
		return ctx.Err()
	case r := <-result:
		err = r.Err
	}
	var notFound *common_errors.NotFoundError
	switch {
	case err == nil:
		return nil
	case errors.Is(err, errBoxStopped), errors.Is(err, errBoxUnavailable), errors.As(err, &notFound):
		return err
	case errors.Is(err, context.DeadlineExceeded):
		slog.WarnContext(ctx, "box did not start within the wake budget", "box", boxID)
	default:
		slog.ErrorContext(ctx, "failed to wake box", "box", boxID, "error", err)
	}
	return nil
}

func (p *Proxy) requestEnsureReady(ctx context.Context, boxID string) error {
	configuration := p.apiclient.GetConfig()
	endpoint, err := url.JoinPath(configuration.Servers[0].URL, "preview", boxID, "ensure-ready")
	if err != nil {
		return fmt.Errorf("build ensure-ready endpoint: %w", err)
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, nil)
	if err != nil {
		return fmt.Errorf("build ensure-ready request: %w", err)
	}
	request.Header.Set("Authorization", configuration.DefaultHeader["Authorization"])
	client := configuration.HTTPClient
	if client == nil {
		client = http.DefaultClient
	}
	response, err := client.Do(request)
	if err != nil {
		return fmt.Errorf("ensure-ready for box %s: %w", boxID, err)
	}
	defer response.Body.Close()
	switch response.StatusCode {
	case http.StatusNoContent, http.StatusOK:
		return nil
	case http.StatusConflict:
		return errBoxStopped
	case http.StatusBadRequest, http.StatusForbidden:
		return errBoxUnavailable
	case http.StatusNotFound:
		return common_errors.NewNotFoundError(fmt.Errorf("box %s not found", boxID))
	default:
		return fmt.Errorf("ensure-ready for box %s: API returned %d", boxID, response.StatusCode)
	}
}

// guestDialFailure turns a dial error into the response the client gets, and
// sets Retry-After on header when retrying the same request can succeed.
func guestDialFailure(header http.Header, err error) *common_errors.CustomError {
	var custom *common_errors.CustomError
	if errors.As(err, &custom) {
		if errors.Is(err, errBoxStarting) {
			header.Set("Retry-After", boxStartingRetryAfter)
		}
		return custom
	}
	var notFound *common_errors.NotFoundError
	if errors.As(err, &notFound) {
		return &common_errors.CustomError{StatusCode: http.StatusNotFound, Message: notFound.Error(), Code: "NOT_FOUND"}
	}
	return &common_errors.CustomError{StatusCode: http.StatusBadGateway, Message: "box unreachable", Code: "BAD_GATEWAY"}
}
