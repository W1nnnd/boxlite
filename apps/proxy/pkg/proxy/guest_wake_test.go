// Copyright 2026 BoxLite AI
// SPDX-License-Identifier: AGPL-3.0

package proxy

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	apiclient "github.com/boxlite-ai/boxlite/libs/api-client-go"
	common_cache "github.com/boxlite-ai/common-go/pkg/cache"
	common_errors "github.com/boxlite-ai/common-go/pkg/errors"
	common_proxy "github.com/boxlite-ai/common-go/pkg/proxy"
	"github.com/gin-gonic/gin"
)

const wakeTestBoxID = "AbCdEf123456"

// fakeRunner answers the runner CONNECT like the real one: 409 box_not_running
// while the box is stopped, 502 while it is up but the guest port does not
// listen, then a guest HTTP server that replies "guest-ok".
type fakeRunner struct {
	listener      net.Listener
	running       atomic.Bool
	refusedDials  atomic.Int32 // 502s left before the service listens
	notRunningHit atomic.Int32
}

func newFakeRunner(t *testing.T) *fakeRunner {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	runner := &fakeRunner{listener: listener}
	t.Cleanup(func() { listener.Close() })
	go func() {
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			go runner.serve(conn)
		}
	}()
	return runner
}

func (r *fakeRunner) serve(conn net.Conn) {
	defer conn.Close()
	reader := bufio.NewReader(conn)
	if _, err := http.ReadRequest(reader); err != nil {
		return
	}
	reply := func(status int, body string) {
		fmt.Fprintf(conn, "HTTP/1.1 %d %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\n\r\n%s",
			status, http.StatusText(status), len(body), body)
	}
	switch {
	case !r.running.Load():
		r.notRunningHit.Add(1)
		reply(http.StatusConflict, `{"error":"box is not running","code":"box_not_running"}`)
	case r.refusedDials.Add(-1) >= 0:
		reply(http.StatusBadGateway, `{"error":"guest tunnel unavailable"}`)
	default:
		io.WriteString(conn, "HTTP/1.1 200 Connection Established\r\n\r\n")
		if _, err := http.ReadRequest(reader); err != nil {
			return
		}
		io.WriteString(conn, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nguest-ok")
	}
}

// fakeAPI serves the proxy-only preview routes. ensure-ready answers with
// status and, on success, starts the fake runner's box.
type fakeAPI struct {
	server        *httptest.Server
	ensureCalls   atomic.Int32
	ensureStatus  int
	startsRunning *fakeRunner
}

func newFakeAPI(t *testing.T, runner *fakeRunner, ensureStatus int) *fakeAPI {
	t.Helper()
	api := &fakeAPI{ensureStatus: ensureStatus, startsRunning: runner}
	api.server = httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		switch request.URL.Path {
		case "/preview/" + wakeTestBoxID + "/ensure-ready":
			api.ensureCalls.Add(1)
			time.Sleep(50 * time.Millisecond) // let concurrent wakes overlap
			if api.ensureStatus == http.StatusNoContent {
				api.startsRunning.running.Store(true)
			}
			writer.WriteHeader(api.ensureStatus)
		case "/preview/" + wakeTestBoxID + "/tunnels/3000":
			writer.WriteHeader(http.StatusOK)
		default:
			writer.WriteHeader(http.StatusCreated) // activity renewals
		}
	}))
	t.Cleanup(api.server.Close)
	return api
}

func newWakeTestProxy(t *testing.T, runner *fakeRunner, api *fakeAPI) *Proxy {
	t.Helper()
	ctx := context.Background()
	runnerCache := common_cache.NewMapCache[RunnerInfo](ctx)
	if err := runnerCache.Set(ctx, wakeTestBoxID, RunnerInfo{ApiUrl: "http://" + runner.listener.Addr().String(), ApiKey: "runner-key"}, time.Minute); err != nil {
		t.Fatal(err)
	}
	publicCache := common_cache.NewMapCache[bool](ctx)
	if err := publicCache.Set(ctx, wakeTestBoxID, true, time.Minute); err != nil {
		t.Fatal(err)
	}
	clientConfig := apiclient.NewConfiguration()
	clientConfig.Servers[0].URL = api.server.URL
	proxy := &Proxy{
		apiclient:                  apiclient.NewAPIClient(clientConfig),
		boxRunnerCache:             runnerCache,
		boxPublicCache:             publicCache,
		boxLastActivityUpdateCache: common_cache.NewMapCache[bool](ctx),
	}
	proxy.guestPortTransport = proxy.newGuestPortTransport()
	return proxy
}

type previewResponse struct {
	Code   int
	Header http.Header
	Body   string
}

// previewGet sends one preview request through the proxy's real HTTP handler.
func previewGet(t *testing.T, proxy *Proxy) previewResponse {
	t.Helper()
	gin.SetMode(gin.TestMode)
	router := gin.New()
	router.Use(common_errors.NewErrorMiddleware(func(_ *gin.Context, err error) common_errors.ErrorResponse {
		return common_errors.ErrorResponse{StatusCode: http.StatusInternalServerError, Message: err.Error()}
	}))
	router.Any("/*path", func(ctx *gin.Context) {
		common_proxy.NewProxyRequestHandler(proxy.GetProxyTarget, nil)(ctx)
		stopActivityPoll(ctx)
	})
	server := httptest.NewServer(router)
	defer server.Close()

	request, err := http.NewRequest(http.MethodGet, server.URL+"/", nil)
	if err != nil {
		t.Fatal(err)
	}
	request.Host = "3000-d-416243644566313233343536.proxy.test"
	response, err := (&http.Client{Timeout: 30 * time.Second}).Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	body, err := io.ReadAll(response.Body)
	if err != nil {
		t.Fatal(err)
	}
	return previewResponse{Code: response.StatusCode, Header: response.Header, Body: string(body)}
}

func TestPreviewRequestWakesStoppedBoxAndWaitsForItsService(t *testing.T) {
	runner := newFakeRunner(t)
	runner.refusedDials.Store(3) // cmd binds its port a few dials after boot
	api := newFakeAPI(t, runner, http.StatusNoContent)
	proxy := newWakeTestProxy(t, runner, api)

	const requests = 20
	var wg sync.WaitGroup
	codes := make([]int, requests)
	bodies := make([]string, requests)
	for i := range requests {
		wg.Add(1)
		go func() {
			defer wg.Done()
			response := previewGet(t, proxy)
			codes[i], bodies[i] = response.Code, response.Body
		}()
	}
	wg.Wait()

	for i := range requests {
		if codes[i] != http.StatusOK || bodies[i] != "guest-ok" {
			t.Fatalf("request %d = %d %q, want 200 guest-ok", i, codes[i], bodies[i])
		}
	}
	if got := api.ensureCalls.Load(); got != 1 {
		t.Fatalf("ensure-ready calls = %d, want 1 for %d concurrent requests", got, requests)
	}
}

func TestPreviewRequestToRunningBoxDoesNotCallEnsureReady(t *testing.T) {
	runner := newFakeRunner(t)
	runner.running.Store(true)
	api := newFakeAPI(t, runner, http.StatusNoContent)

	response := previewGet(t, newWakeTestProxy(t, runner, api))

	if response.Code != http.StatusOK || api.ensureCalls.Load() != 0 {
		t.Fatalf("response %d, ensure-ready calls %d; want 200 and 0", response.Code, api.ensureCalls.Load())
	}
}

func TestPreviewRequestPastWakeBudgetAsksClientToRetry(t *testing.T) {
	previous := guestWakeBudget
	guestWakeBudget = 600 * time.Millisecond
	t.Cleanup(func() { guestWakeBudget = previous })

	runner := newFakeRunner(t)
	runner.refusedDials.Store(1000) // the service never comes up in time
	api := newFakeAPI(t, runner, http.StatusNoContent)

	response := previewGet(t, newWakeTestProxy(t, runner, api))

	assertErrorResponse(t, response, http.StatusServiceUnavailable, "BOX_STARTING")
	if got := response.Header.Get("Retry-After"); got != boxStartingRetryAfter {
		t.Fatalf("Retry-After = %q, want %q", got, boxStartingRetryAfter)
	}
}

func TestPreviewRequestReportsBoxesThatWillNotStart(t *testing.T) {
	for _, tc := range []struct {
		name         string
		ensureStatus int
		wantStatus   int
		wantCode     string
	}{
		{"auto-resume disabled", http.StatusConflict, http.StatusServiceUnavailable, "BOX_STOPPED"},
		{"organization suspended", http.StatusForbidden, http.StatusConflict, "BOX_UNAVAILABLE"},
		{"box in error", http.StatusBadRequest, http.StatusConflict, "BOX_UNAVAILABLE"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			runner := newFakeRunner(t)
			api := newFakeAPI(t, runner, tc.ensureStatus)

			response := previewGet(t, newWakeTestProxy(t, runner, api))

			assertErrorResponse(t, response, tc.wantStatus, tc.wantCode)
			if response.Header.Get("Retry-After") != "" {
				t.Fatal("Retry-After set for a box that retrying cannot start")
			}
			if hits := runner.notRunningHit.Load(); hits != 1 {
				t.Fatalf("runner dials = %d, want 1 (no polling after a final answer)", hits)
			}
		})
	}
}

func TestTunnelConnectTakesTheWakePath(t *testing.T) {
	runner := newFakeRunner(t)
	api := newFakeAPI(t, runner, http.StatusConflict)
	proxy := newWakeTestProxy(t, runner, api)

	request := httptest.NewRequest(http.MethodConnect, "http://proxy.test", nil)
	request.Host = "3000-d-416243644566313233343536.proxy.test:443"
	response := httptest.NewRecorder()
	proxy.handleTunnelConnect(response, request)

	if response.Code != http.StatusServiceUnavailable || api.ensureCalls.Load() != 1 {
		t.Fatalf("CONNECT = %d with %d ensure-ready calls; want 503 after one wake", response.Code, api.ensureCalls.Load())
	}
}

// An unexpected ensure-ready failure is not a final answer: the start may still
// be under way, so the request keeps polling and ends as a retryable 503.
func TestPreviewRequestKeepsPollingAfterAnUnexpectedWakeError(t *testing.T) {
	previous := guestWakeBudget
	guestWakeBudget = 600 * time.Millisecond
	t.Cleanup(func() { guestWakeBudget = previous })

	runner := newFakeRunner(t)
	api := newFakeAPI(t, runner, http.StatusInternalServerError)

	response := previewGet(t, newWakeTestProxy(t, runner, api))

	assertErrorResponse(t, response, http.StatusServiceUnavailable, "BOX_STARTING")
	if hits := runner.notRunningHit.Load(); hits < 2 {
		t.Fatalf("runner dials = %d, want the dial retried after the failed wake", hits)
	}
	if calls := api.ensureCalls.Load(); calls != 1 {
		t.Fatalf("ensure-ready calls = %d, want 1", calls)
	}
}

func assertErrorResponse(t *testing.T, response previewResponse, wantStatus int, wantCode string) {
	t.Helper()
	var body common_errors.ErrorResponse
	if err := json.Unmarshal([]byte(response.Body), &body); err != nil {
		t.Fatalf("response %d body %q is not a JSON error: %v", response.Code, response.Body, err)
	}
	if response.Code != wantStatus || body.Code != wantCode {
		t.Fatalf("response = %d %s, want %d %s", response.Code, body.Code, wantStatus, wantCode)
	}
}
