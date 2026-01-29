#!/bin/bash
# Helper script for testing Flame with local installation

set -e

INSTALL_PREFIX="${INSTALL_PREFIX:-/tmp/flame-dev}"
FLAME_ENDPOINT="${FLAME_ENDPOINT:-http://127.0.0.1:8080}"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if flmadm is built
if [ ! -f "target/release/flmadm" ]; then
    log_info "Building flmadm..."
    cargo build --release -p flmadm
fi

# Parse command
case "$1" in
    install)
        log_info "Installing Flame to $INSTALL_PREFIX..."
        make install-dev INSTALL_PREFIX="$INSTALL_PREFIX"
        ;;
    
    start)
        log_info "Starting Flame services..."
        
        # Start session manager
        log_info "Starting session manager..."
        "$INSTALL_PREFIX/bin/flame-session-manager" \
            --config "$INSTALL_PREFIX/conf/flame-cluster.yaml" \
            > "$INSTALL_PREFIX/logs/fsm.log" 2>&1 &
        echo $! > /tmp/flame-fsm.pid
        
        # Wait for session manager
        sleep 3
        
        # Start executor manager
        log_info "Starting executor manager..."
        "$INSTALL_PREFIX/bin/flame-executor-manager" \
            --config "$INSTALL_PREFIX/conf/flame-cluster.yaml" \
            > "$INSTALL_PREFIX/logs/fem.log" 2>&1 &
        echo $! > /tmp/flame-fem.pid
        
        # Wait for services to be ready
        sleep 3
        
        # Check if services are running
        if ps -p $(cat /tmp/flame-fsm.pid) > /dev/null; then
            log_info "Session manager started (PID: $(cat /tmp/flame-fsm.pid))"
        else
            log_error "Session manager failed to start"
            cat "$INSTALL_PREFIX/logs/fsm.log"
            exit 1
        fi
        
        if ps -p $(cat /tmp/flame-fem.pid) > /dev/null; then
            log_info "Executor manager started (PID: $(cat /tmp/flame-fem.pid))"
        else
            log_error "Executor manager failed to start"
            cat "$INSTALL_PREFIX/logs/fem.log"
            exit 1
        fi
        
        log_info "Flame is running at $FLAME_ENDPOINT"
        ;;
    
    stop)
        log_info "Stopping Flame services..."
        
        if [ -f /tmp/flame-fem.pid ]; then
            PID=$(cat /tmp/flame-fem.pid)
            if ps -p $PID > /dev/null; then
                kill $PID
                log_info "Stopped executor manager (PID: $PID)"
            fi
            rm -f /tmp/flame-fem.pid
        fi
        
        if [ -f /tmp/flame-fsm.pid ]; then
            PID=$(cat /tmp/flame-fsm.pid)
            if ps -p $PID > /dev/null; then
                kill $PID
                log_info "Stopped session manager (PID: $PID)"
            fi
            rm -f /tmp/flame-fsm.pid
        fi
        ;;
    
    restart)
        log_info "Restarting Flame services..."
        $0 stop
        sleep 2
        $0 start
        ;;
    
    status)
        log_info "Checking Flame services status..."
        
        if [ -f /tmp/flame-fsm.pid ] && ps -p $(cat /tmp/flame-fsm.pid) > /dev/null; then
            log_info "Session manager is running (PID: $(cat /tmp/flame-fsm.pid))"
        else
            log_warn "Session manager is not running"
        fi
        
        if [ -f /tmp/flame-fem.pid ] && ps -p $(cat /tmp/flame-fem.pid) > /dev/null; then
            log_info "Executor manager is running (PID: $(cat /tmp/flame-fem.pid))"
        else
            log_warn "Executor manager is not running"
        fi
        ;;
    
    logs)
        log_info "Showing service logs..."
        echo ""
        echo "=== Session Manager Logs ==="
        tail -50 "$INSTALL_PREFIX/logs/fsm.log" 2>/dev/null || log_warn "No FSM logs found"
        echo ""
        echo "=== Executor Manager Logs ==="
        tail -50 "$INSTALL_PREFIX/logs/fem.log" 2>/dev/null || log_warn "No FEM logs found"
        ;;
    
    test)
        log_info "Running E2E tests against local cluster..."
        export FLAME_ENDPOINT="$FLAME_ENDPOINT"
        make e2e-py-local
        ;;
    
    uninstall)
        log_info "Uninstalling Flame from $INSTALL_PREFIX..."
        make uninstall-dev INSTALL_PREFIX="$INSTALL_PREFIX"
        ;;
    
    clean)
        log_info "Cleaning up..."
        $0 stop
        $0 uninstall
        ;;
    
    *)
        echo "Usage: $0 {install|start|stop|restart|status|logs|test|uninstall|clean}"
        echo ""
        echo "Commands:"
        echo "  install    - Install Flame locally"
        echo "  start      - Start Flame services"
        echo "  stop       - Stop Flame services"
        echo "  restart    - Restart Flame services"
        echo "  status     - Check service status"
        echo "  logs       - Show service logs"
        echo "  test       - Run E2E tests"
        echo "  uninstall  - Uninstall Flame"
        echo "  clean      - Stop and uninstall Flame"
        echo ""
        echo "Environment variables:"
        echo "  INSTALL_PREFIX - Installation directory (default: /tmp/flame-dev)"
        echo "  FLAME_ENDPOINT - Flame endpoint URL (default: http://127.0.0.1:8080)"
        exit 1
        ;;
esac
