;;; -*- lexical-binding: t -*-
;;; SPDX-License-Identifier: GPL-3.0-or-later
;;; Tests use native-module stubs, so they also run without a graphical session.

(require 'ert)
(require 'cl-lib)
(provide 'libreka)
(require 'reka-simulation)

(ert-deftest reka-simulation-modifier-and-character-encoding ()
  (dolist (case '(("C-a" [97 4]) ("A" [65 1]) ("C-S-a" [65 5])
                  ("M-a" [97 8]) ("s-a" [97 64])
                  ("TAB" ["Tab" 0]) ("RET" ["Return" 0])
                  ("ESC" ["Escape" 0]) ("DEL" ["BackSpace" 0])
                  ("<left>" ["left" 0]) ("S-<left>" ["left" 1])
                  ("🙂" [128578 0])))
    (should (equal (reka-simulation--event (aref (kbd (car case)) 0))
                   (cadr case)))))

(ert-deftest reka-simulation-requires-an-application-and-capability ()
  (with-temp-buffer
    (should-error (reka-send-keys "C-a") :type 'user-error)
    (let ((reka-handle 'handle)
          (reka-window 'window)
          (major-mode 'reka-mode))
      (cl-letf (((symbol-function 'reka-virtual-keyboard-available-p) (lambda (_) nil)))
        (should-error (reka-send-keys "C-a") :type 'user-error)))))

(ert-deftest reka-simulation-explicit-destination-and-bounded-sequence ()
  (with-temp-buffer
    (let ((reka-handle 'handle)
          (reka-window 'destination)
          (major-mode 'reka-mode)
          delivered)
      (cl-letf (((symbol-function 'reka-virtual-keyboard-available-p) (lambda (_) t))
                ((symbol-function 'reka-inject-key-sequence)
                 (lambda (&rest arguments) (setq delivered arguments))))
        (reka-send-keys "C-a C-c")
        (should (equal delivered '(handle destination [[97 4] [99 4]])))
        (should-error (reka-send-keys []) :type 'user-error)
        (should-error (reka-send-keys (make-vector 65 ?a)) :type 'user-error)
        (should-error (reka-send-keys [(mouse-1)] ) :type 'user-error)))))

(ert-deftest reka-simulation-binding-is-confined-to-reka-mode ()
  (let ((reka-handle 'handle)
        (reka-mode-map (make-sparse-keymap))
        (global-before (global-key-binding (kbd "C-f")))
        intercepted)
    (cl-letf (((symbol-function 'reka-push-intercept-prefix)
               (lambda (key) (setq intercepted key))))
      (reka-define-simulation-key "C-f" "<right>")
      (should (equal intercepted "C-f"))
      (should (commandp (lookup-key reka-mode-map (kbd "C-f"))))
      (should (equal (global-key-binding (kbd "C-f")) global-before)))))

(ert-deftest reka-simulation-definitions-include-detached-floating-map ()
  (let ((reka-handle 'handle)
        (reka-mode-map (make-sparse-keymap)))
    (cl-progv '(reka-floating-command-map) (list (make-sparse-keymap))
      (cl-letf (((symbol-function 'reka-push-intercept-prefix) (lambda (_) nil)))
        (reka-define-simulation-key "C-f" "<right>")
        (should (eq (lookup-key reka-mode-map (kbd "C-f"))
                    (lookup-key (symbol-value 'reka-floating-command-map) (kbd "C-f"))))))))

(ert-deftest reka-simulation-targets-the-focused-detached-float ()
  (let ((floating (generate-new-buffer " *reka-floating-test*"))
        (reka-handle 'handle)
        delivered)
    (unwind-protect
        (with-temp-buffer
          (with-current-buffer floating
            (setq major-mode 'reka-mode)
            (setq-local reka-window 'floating-window))
          (cl-letf (((symbol-function 'reka--active-application-buffer) (lambda () floating))
                    ((symbol-function 'reka-virtual-keyboard-available-p) (lambda (_) t))
                    ((symbol-function 'reka-inject-key-sequence)
                     (lambda (&rest arguments) (setq delivered arguments))))
            (reka-send-keys "C-v")
            (should (equal delivered '(handle floating-window [[118 4]])))))
      (kill-buffer floating))))

(ert-run-tests-batch-and-exit)
