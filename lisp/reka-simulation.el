;;; reka-simulation.el --- Simulate application keys through River -*- lexical-binding: t -*-
;; SPDX-License-Identifier: GPL-3.0-or-later

;;; Commentary:
;; This optional extension sends keys to the application represented by the
;; selected Reka buffer.  River 0.5 commits its focus and the keyboard events
;; atomically; older compositors continue to work without simulation.
;;
;; Example, after `reka-enable':
;;   (reka-define-simulation-key "C-f" "<right>")
;;   (reka-define-simulation-key "C-b" "<left>")
;;   (reka-define-simulation-key "C-y" "C-v")
;; These bindings only change Reka application buffers, not Emacs text buffers.
;; No editing keys are intercepted merely by loading this library.

;;; Code:

(require 'reka)

(defvar reka-floating-command-map)
(declare-function reka--active-application-buffer "reka")
(declare-function reka-virtual-keyboard-available-p "libreka" (handle))
(declare-function reka-inject-key-sequence "libreka" (handle window events))

(defconst reka-simulation--modifiers
  '((shift . 1) (control . 4) (meta . 8) (alt . 8) (super . 64) (hyper . 128)))

(defun reka-simulation--event (event)
  "Convert keyboard EVENT into a [KEY MODIFIERS] native-module stroke.
KEY is a final Unicode character or XKB keysym name, independent of the
physical keyboard layout.  Shifted letters are therefore converted to their
uppercase keysyms.  Explicit shifted punctuation, such as S-1, retains its
literal keysym and modifier; use the resulting character, such as !, for text."
  (unless (or (integerp event) (symbolp event))
    (user-error "Only keyboard events can be simulated"))
  (let* ((basic (event-basic-type event))
         (modifiers (event-modifiers event))
         ;; Emacs encodes TAB, RET, ESC and DEL as character aliases of control
         ;; events.  Restore their conventional application key identities.
         (special (and (integerp event)
                       (alist-get (logand event #x1fffff)
                                  '((9 . "Tab") (13 . "Return")
                                    (27 . "Escape") (127 . "BackSpace")))))
         (key (cond
               (special
                (setq modifiers (remq 'control modifiers))
                special)
               ((integerp basic)
                (if (memq 'shift modifiers) (upcase basic) basic))
               ((eq basic 'backtab) "ISO_Left_Tab")
               ((symbolp basic)
                (replace-regexp-in-string "-" "_" (symbol-name basic)))
               (t (user-error "Only keyboard events can be simulated"))))
         (bits 0))
    (dolist (modifier modifiers)
      (let ((bit (alist-get modifier reka-simulation--modifiers)))
        (unless bit (user-error "Unsupported key modifier: %s" modifier))
        (setq bits (logior bits bit))))
    (vector key bits)))

(defun reka-send-keys (keys)
  "Send KEYS to the focused floating application or selected Reka buffer.
KEYS is a string understood by `kbd', or a vector of keyboard events.  The
sequence must contain 1 to 64 keys.  Each press is released, and simulated
modifiers are cleared afterward.  The physical keyboard state is untouched.

This requests focus for that explicit displayed application and injects its
keys in the same River manage sequence.  A closed or hidden destination is
discarded; keys are never redirected to another application's current focus.
Queued simulations are discarded when the session locks or its seat is removed.

Requires River's optional `river-virtual-keyboard-v1' protocol (River 0.5).
Loading this library does not enable any global editing-key interceptions."
  (interactive (list (read-key-sequence-vector "Send application keys: ")))
  (let ((buffer (if (fboundp 'reka--active-application-buffer)
                    (reka--active-application-buffer)
                  (and (derived-mode-p 'reka-mode) (current-buffer)))))
    (unless (and (buffer-live-p buffer) reka-handle)
      (user-error "Select a Reka application buffer first"))
    (with-current-buffer buffer
      (unless (and (derived-mode-p 'reka-mode) reka-window)
        (user-error "Select a Reka application buffer first"))
      (unless (reka-virtual-keyboard-available-p reka-handle)
	(user-error "Keyboard simulation requires River 0.5 and an active unlocked seat"))
      (let ((events (cond ((stringp keys) (vconcat (kbd keys)))
			  ((vectorp keys) keys)
			  (t (user-error "Keys must be a kbd string or keyboard-event vector")))))
	(unless (<= 1 (length events) 64)
	  (user-error "Send between 1 and 64 keys at a time"))
	(reka-inject-key-sequence
	 reka-handle reka-window (vconcat (mapcar #'reka-simulation--event events)))))))

(defun reka-define-simulation-key (key replacement)
  "Map KEY to application REPLACEMENT in Reka buffers.
KEY must be a single keyboard event in `kbd' notation.  REPLACEMENT is a
`kbd' string or event vector accepted by `reka-send-keys'.  Register this
after `reka-enable', as with `reka-push-intercept-prefix'.  Prefix interception
forwards KEY to Emacs; only Reka application buffers use the replacement."
  (unless reka-handle (user-error "Enable Reka before defining simulation keys"))
  (unless (= (length (kbd key)) 1)
    (user-error "A simulation binding must intercept one keyboard event"))
  (reka-push-intercept-prefix key)
  (let ((command (lambda () (interactive) (reka-send-keys replacement))))
    (define-key reka-mode-map (kbd key) command)
    (when (boundp 'reka-floating-command-map)
      (define-key reka-floating-command-map (kbd key) command))))

(provide 'reka-simulation)
;;; reka-simulation.el ends here
