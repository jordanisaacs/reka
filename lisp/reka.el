;;; -*- lexical-binding: t -*-
;;; SPDX-License-Identifier: GPL-3.0-or-later

(require 'libreka)
(require 'cl-lib)
(require 'seq)
(require 'tab-bar)

(defgroup reka nil
  "Reka - Emacs swimming in the river"
  :group 'environment
  :prefix "reka-")

(defvar reka-handle nil
  "Opaque handle for interacting with the WM")

(defcustom reka-enable-hook nil
  "Hook run at the end of `reka-enable'."
  :type 'hook)

(defvar reka-ready nil
  "Non-nil after River has processed the first usable desktop frame.
Unlike `reka-enable-hook', this indicates asynchronous native startup completed.")

(defcustom reka-ready-hook nil
  "Hook run once after River processes the first usable desktop frame.
Session managers can finalize startup here.  The hook runs in Emacs after its
native window manager has a displayed frame, input seat and key bindings."
  :type 'hook)

(defun reka--set-frame-name (frame)
  (unless (string-prefix-p "reka-frame-"
                           (frame-parameter frame 'name))
    (set-frame-parameter frame 'name (make-temp-name "reka-frame-"))))

(defun reka--ensure-frame-names ()
  "Ensure each frame has a unique title that reka can match to its window."
  (cl-loop for frame being the frames
           do (reka--set-frame-name frame)))

(defun reka--find-buffer-for-window (window)
  (seq-find (lambda (buf)
              (when-let* ((w (buffer-local-value 'reka-window buf)))
                (reka-window-equal w window)))
            (reka--list-buffers)))

(defvar reka-processing-commands (make-mutex "reka-processor-lock")
  ;; Honestly, I'm not sure if this lock is ever contended. In theory there
  ;; should only ever be one linear thread doing stuff in Emacs. However, the
  ;; signal handler and its effects are a bit odd, so I'm opting for a defensive
  ;; approach here.
  "Internal lock to avoid double-processing of commands in signal handler.")

(defvar-local reka-app-id nil)

(defvar reka--last-focused nil)
(defvar reka-intercept-prefixes)

(defvar reka--focused-floating-buffer nil
  "Native-focused floating buffer, which has no reserved Emacs tile.")

(defvar reka--floating-keymap-active nil
  "Non-nil while floating application bindings may override ordinary keys.")

(defvar reka--adjusting-floating-layout nil)
(defvar reka--floating-selection-context nil
  "Selected frame, window and buffer when native floating focus was assigned.
Deferred redisplay notifications for that unchanged context are not explicit
Emacs selections, including notifications caused by detaching a tiled slot.")
(defvar-local reka--floating-tab nil)
(defvar-local reka-parent-window nil)

(defvar-local reka-floating nil
  "Non-nil when this buffer's native window is floating.")

(defvar-local reka-identifier nil
  "Stable compositor identifier of this application window.
Matches the corresponding ext-foreign-toplevel-list identifier; unlike an
application title or PID, it distinguishes windows across manager handoffs.")

(defvar-local reka-title nil
  "The application's original window title, without buffer-name decoration.")

(defvar-local reka-floating-geometry nil
  "Last native window geometry as (X Y WIDTH HEIGHT), in logical pixels.
Coordinates are relative to the usable area of `reka-floating-frame-name'.")

(defvar-local reka-floating-frame-name nil
  "Name of the Emacs frame displaying this native window.")

(defcustom reka-window-created-hook nil
  "Hook run in a newly created reka buffer."
  :type 'hook)

(defcustom reka-window-geometry-change-hook nil
  "Hook run in a reka buffer after its native geometry changes."
  :type 'hook)

(defcustom reka-floating-state-change-hook nil
  "Hook run in a reka buffer when its native floating state changes."
  :type 'hook)

(defcustom reka-floating-default-size '(640 . 480)
  "Initial width and height of manually floated windows, in logical pixels.
Dialog windows initially use their client's natural dimensions."
  :type '(cons integer integer))

(defcustom reka-floating-step 32
  "Number of logical pixels moved or resized by floating keyboard commands."
  :type 'integer)

(defcustom reka-floating-toggle-key "s-f"
  "Global key for toggling the focused native window's floating state.
Set to nil to register no default; `reka-floating-toggle' remains available."
  :type '(choice (const nil) key-sequence))

(defun reka--make-buffer-name (app-id title)
  (let ((title-trunc (if (> (length title) 40)
                         (format "%s…" (substring title 0 40))
                       title)))
    (if app-id (concat title-trunc " - " app-id)
      title-trunc)))

(defun reka--handle-commands ()
  (interactive)

  (with-mutex reka-processing-commands
    (setq reka--last-focused nil)
    (while-let ((cmd (reka-get-next-command reka-handle)))
      (pcase cmd
        ('ready
         (unless reka-ready
           (setq reka-ready t)
           (run-hooks 'reka-ready-hook)))

        (`(key-event . ,key)
         (push (cons t key) unread-command-events))

        (`(new-window ,window ,floating ,parent)
         (reka--create-buffer window floating parent)
         (reka-notify-buffer-created reka-handle window))

        (`(window-closed . ,window)
         (when-let* ((buf (reka--find-buffer-for-window window)))
           (kill-buffer buf)))

        (`(window-geometry ,window ,floating ,frame-name ,x ,y ,w ,h)
         (when-let* ((buf (reka--find-buffer-for-window window)))
           (reka--apply-window-geometry buf floating frame-name (list x y w h))))

        (`(parent-changed ,window ,parent)
         (when-let* ((buf (reka--find-buffer-for-window window)))
           (with-current-buffer buf
             (setq reka-parent-window parent)
             (when reka--floating-tab (reka--inherit-parent-location)))))

        (`(frame-focused . ,frame-name)
         (when-let* ((frame (reka--frame-by-name frame-name)))
           (select-frame frame 'norecord))
         (setq reka--focused-floating-buffer nil
               reka--floating-keymap-active nil))

        (`(minimize-requested . ,window)
         (when-let* ((buf (reka--find-buffer-for-window window)))
           (with-current-buffer buf
             (if reka-floating
                 (setq reka--floating-tab nil)
               (bury-buffer)))
           (when (eq buf reka--focused-floating-buffer)
             (setq reka--focused-floating-buffer nil
		   reka--floating-keymap-active nil))))

        (`(focused ,window ,frame-name)
         (when-let* ((buf (reka--find-buffer-for-window window))
                     (frame (reka--frame-by-name frame-name)))
           ;; Native output ownership is authoritative. PGTK can keep the same
           ;; buffer in a spare frame; scanning every frame can select that
           ;; invisible copy after a click on the real displayed application.
           (select-frame frame 'norecord)
           (if (buffer-local-value 'reka-floating buf)
               (progn
                 (setq reka--focused-floating-buffer buf)
                 (reka--remember-floating-selection))
             (setq reka--focused-floating-buffer nil
		   reka--floating-keymap-active nil)
             (when-let* ((win (get-buffer-window buf frame)))
               (select-window win 'norecord)))))

        (`(app-id-change ,window ,app-id)
         (when-let* ((buf (reka--find-buffer-for-window window)))
           (with-current-buffer buf
             (setq reka-app-id app-id))))

        (`(identifier-change ,window ,identifier)
         (when-let* ((buf (reka--find-buffer-for-window window)))
           (with-current-buffer buf
             (setq reka-identifier identifier))))

        (`(title-change ,window ,title)
         (when-let* ((buf (reka--find-buffer-for-window window)))
           (with-current-buffer buf
             (setq reka-title title)
             (rename-buffer (reka--make-buffer-name reka-app-id title) t))))

        ('frame-request (make-frame))

        (`(message . ,msg)
         (message "reka: %s" msg))

        (_ (error "received unknown command from reka: %s" cmd))))

    ;; Publish after creating buffers: detached dialogs do not change Emacs'
    ;; window layout and therefore do not trigger its configuration hook.
    (reka--refresh-floating-keymap)
    (reka-update-window-parameters reka-handle (reka--all-window-parameters))
    (run-at-time nil nil #'reka--update-focus-request)))

(defun reka--handle-event (&rest _) ;; unused process filter args
  (when reka-handle
    (run-at-time 0 nil #'reka--handle-commands)))

;; Major mode for reka-managed buffers
(defvar-local reka-window nil
  "Window object for this reka-mode buffer.")

(defun reka--buffer-killed ()
  (when (eq (current-buffer) reka--focused-floating-buffer)
    (setq reka--focused-floating-buffer nil
          reka--floating-keymap-active nil))
  (when reka-window
    (reka-close-window reka-handle reka-window)))

(define-derived-mode reka-mode special-mode "Reka"
  "Major mode for buffers representing windows managed by reka."
  :group 'reka
  (setq-local buffer-read-only t)
  (add-hook 'kill-buffer-hook #'reka--buffer-killed nil t)
  (scroll-bar-mode 0)
  (setq-local left-fringe-width 0
              right-fringe-width 0))

(defun reka--refresh-floating-keymap ()
  "Keep application bindings out of minibuffers and recursive Emacs editing."
  (setq reka--floating-keymap-active
        (and (buffer-live-p reka--focused-floating-buffer)
             (reka--floating-visible-p reka--focused-floating-buffer)
             (zerop (minibuffer-depth))
             (zerop (recursion-depth)))))

(defun reka--selected-context ()
  "Return the selected frame, window and its displayed buffer."
  (let ((window (selected-window)))
    (list (selected-frame) window (window-buffer window))))

(defun reka--remember-floating-selection ()
  "Record the underlying Emacs selection after assigning floating focus."
  (setq reka--floating-selection-context (reka--selected-context)))

(defun reka--active-application-buffer ()
  "Return the native-focused floating buffer or the current tiled application."
  (cond
   ((and (buffer-live-p reka--focused-floating-buffer)
         (reka--floating-visible-p reka--focused-floating-buffer))
    reka--focused-floating-buffer)
   ((derived-mode-p 'reka-mode) (current-buffer))))

(defun reka--require-window ()
  "Return the active native window, or signal an interactive error."
  (let ((buffer (reka--active-application-buffer)))
    (unless (and reka-handle buffer
                 (buffer-local-value 'reka-window buffer))
      (user-error "There is no active Reka application window"))
    (buffer-local-value 'reka-window buffer)))

(defun reka-floating-toggle ()
  "Toggle this buffer's native window between tiled and floating placement."
  (interactive)
  (reka-toggle-window-floating reka-handle (reka--require-window)))

(defun reka-request-close (&optional buffer)
  "Ask BUFFER's application to close without destroying its Emacs buffer.
The application may open a save dialog or decline.  Its layout and input
remain available until the compositor reports that the window has closed."
  (interactive)
  (if buffer
      (with-current-buffer buffer
        (unless (and reka-handle reka-window)
          (user-error "There is no Reka application in this buffer"))
        (reka-close-window reka-handle reka-window))
    (reka-close-window reka-handle (reka--require-window))))

(defun reka-floating-center ()
  "Float and center this window over its parent or the current output."
  (interactive)
  (reka-center-floating-window reka-handle (reka--require-window)))

(defun reka-floating-move (dx dy)
  "Float and move this window by DX and DY logical pixels."
  (interactive "nHorizontal displacement: \nnVertical displacement: ")
  (reka-move-floating-window reka-handle (reka--require-window) dx dy))

(defun reka-floating-resize (dw dh)
  "Float and resize this window by DW and DH logical pixels."
  (interactive "nWidth change: \nnHeight change: ")
  (reka-resize-floating-window reka-handle (reka--require-window) dw dh))

(defun reka-floating-move-left ()
  (interactive)
  (reka-floating-move (- reka-floating-step) 0))

(defun reka-floating-move-right ()
  (interactive)
  (reka-floating-move reka-floating-step 0))

(defun reka-floating-move-up ()
  (interactive)
  (reka-floating-move 0 (- reka-floating-step)))

(defun reka-floating-move-down ()
  (interactive)
  (reka-floating-move 0 reka-floating-step))

(defun reka-floating-shrink-width ()
  (interactive)
  (reka-floating-resize (- reka-floating-step) 0))

(defun reka-floating-grow-width ()
  (interactive)
  (reka-floating-resize reka-floating-step 0))

(defun reka-floating-shrink-height ()
  (interactive)
  (reka-floating-resize 0 (- reka-floating-step)))

(defun reka-floating-grow-height ()
  (interactive)
  (reka-floating-resize 0 reka-floating-step))

(defvar-keymap reka-floating-map
  :doc "Floating window commands, available under C-x w in reka buffers."
  "f" #'reka-floating-toggle
  "c" #'reka-floating-center
  "<left>" #'reka-floating-move-left
  "<right>" #'reka-floating-move-right
  "<up>" #'reka-floating-move-up
  "<down>" #'reka-floating-move-down
  "S-<left>" #'reka-floating-shrink-width
  "S-<right>" #'reka-floating-grow-width
  "S-<up>" #'reka-floating-shrink-height
  "S-<down>" #'reka-floating-grow-height)

(define-key reka-mode-map (kbd "C-x w") reka-floating-map)

(defvar-keymap reka-floating-command-map
  :doc "Floating commands while a detached native application is focused."
  "C-x w" reka-floating-map)

(defvar reka--floating-emulation-map-alist
  `((reka--floating-keymap-active . ,reka-floating-command-map)))

(defun reka--is-reka-buffer (buf)
  (with-current-buffer buf
    (eq major-mode 'reka-mode)))

(defun reka--get-window (buffer)
  (buffer-local-value 'reka-window buffer))

(defun reka--list-buffers ()
  (apply #'vector (seq-filter #'reka--is-reka-buffer (buffer-list))))

(defun reka--window-parameters (frame window)
  (let ((edges (window-inside-pixel-edges window))
        (buffer (window-buffer window)))
    ;; A buffer can be reused in another tab with identical native geometry.
    ;; Update workspace metadata here even when no render notification changes.
    (with-current-buffer buffer
      (setq reka-floating-frame-name frame
            reka--floating-tab (reka--current-tab-id (window-frame window))))
    (reka-make-window-parameters
     (buffer-local-value 'reka-window buffer)
     frame
     (nth 0 edges)
     (nth 1 edges)
     (- (nth 2 edges) (nth 0 edges))
     (- (nth 3 edges) (nth 1 edges)))))

(defun reka--frame-by-name (name)
  (seq-find (lambda (frame) (equal name (frame-parameter frame 'name)))
            (frame-list)))

(defun reka--current-tab-id (&optional frame)
  "Return a stable private ID for the current tab of FRAME."
  (let* ((frame (or frame (selected-frame)))
         (tab (tab-bar--current-tab-find nil frame)))
    (or (alist-get 'reka-id tab)
        (setf (alist-get 'reka-id (cdr tab)) (gensym "reka-tab-")))))

(defun reka--inherit-parent-location (&optional depth)
  "Adopt a native parent's frame/tab without allocating an Emacs window."
  (when-let* ((parent (and (< (or depth 0) (length (buffer-list))) reka-parent-window))
              (buffer (reka--find-buffer-for-window parent)))
    (with-current-buffer buffer
      (when (and reka-floating reka--floating-tab)
        (reka--inherit-parent-location (1+ (or depth 0)))))
    (let* ((window (get-buffer-window buffer t))
           (frame (when window (window-frame window))))
      (setq reka-floating-frame-name
            (or (and frame (frame-parameter frame 'name))
                (buffer-local-value 'reka-floating-frame-name buffer)
                reka-floating-frame-name)
            reka--floating-tab
            (or (and frame (reka--current-tab-id frame))
                (buffer-local-value 'reka--floating-tab buffer)
                reka--floating-tab)))))

(defun reka--new-tab (tab)
  "Give a newly created TAB its own floating-window membership."
  (setf (alist-get 'reka-id (cdr tab)) (gensym "reka-tab-"))
  (setq reka--focused-floating-buffer nil
        reka--floating-keymap-active nil)
  (reka--handle-event))

(defun reka--tab-selected (&rest _)
  (setq reka--focused-floating-buffer nil
        reka--floating-keymap-active nil)
  (reka--handle-event))

(defun reka--floating-visible-p (buffer)
  (when (buffer-live-p buffer)
    (with-current-buffer buffer
      (when (and reka-floating reka--floating-tab)
        (reka--inherit-parent-location))
      (when-let* ((frame (and reka-floating
                              (reka--frame-by-name reka-floating-frame-name))))
        (eq reka--floating-tab (reka--current-tab-id frame))))))

(defun reka--detach-floating-buffer (buffer)
  "Remove BUFFER's tiled slots while preserving its native frame and tab."
  (let ((reka--adjusting-floating-layout t))
    (dolist (window (get-buffer-window-list buffer nil t))
      (when (window-live-p window)
        (let ((frame (window-frame window)))
          (with-current-buffer buffer
            (setq reka-floating-frame-name (frame-parameter frame 'name)
                  reka--floating-tab (reka--current-tab-id frame))))
        (if (window-parent window)
            (delete-window window)
          (switch-to-prev-buffer window 'bury)
          (when (with-current-buffer (window-buffer window)
                  (and (derived-mode-p 'reka-mode) reka-floating))
            (set-window-buffer window (get-buffer-create "*reka-background*")))))))
  (with-current-buffer buffer
    (reka--inherit-parent-location))
  (setq reka--focused-floating-buffer buffer)
  (reka--remember-floating-selection)
  (reka--refresh-floating-keymap))

(defun reka--apply-window-geometry (buffer floating frame-name geometry)
  "Apply native placement metadata without reserving an Emacs tile for floats."
  (with-current-buffer buffer
    (let ((changed (not (eq reka-floating floating)))
          (reka--adjusting-floating-layout t))
      (setq reka-floating floating
            reka-floating-geometry geometry
            reka-floating-frame-name frame-name)
      (if floating
          (progn
            (when changed
              (unless reka--floating-tab
                (when-let* ((frame (reka--frame-by-name frame-name)))
                  (setq reka--floating-tab (reka--current-tab-id frame))))
              (reka--detach-floating-buffer buffer))
            (when (or changed reka--floating-tab)
              (reka--inherit-parent-location)))
        (when-let* ((frame (reka--frame-by-name frame-name)))
          (when (or changed (get-buffer-window buffer t))
            (setq reka--floating-tab (reka--current-tab-id frame)))
          (when changed
            (when (eq buffer reka--focused-floating-buffer)
              (setq reka--focused-floating-buffer nil
		    reka--floating-keymap-active nil))
            (with-selected-frame frame
              (when-let* ((window (display-buffer buffer)))
                (select-window window 'norecord))))))
      (when changed (run-hooks 'reka-floating-state-change-hook))
      (run-hooks 'reka-window-geometry-change-hook))))

(defun reka--all-window-parameters ()
  "Collect tiled rectangles and visible detached floats for the native manager."
  (apply #'vector
         (cl-loop for frame being the frames
                  for frame-name = (frame-parameter frame 'name)
                  collect
                  (apply #'vector
                         (append
                          (cl-loop for window in (window-list frame)
                                   for buffer = (window-buffer window)
                                   when (and (reka--is-reka-buffer buffer)
                                             (not (buffer-local-value 'reka-floating buffer)))
                                   collect (reka--window-parameters frame-name window))
                          (cl-loop for buffer across (reka--list-buffers)
                                   when (and (reka--floating-visible-p buffer)
                                             (equal frame-name (buffer-local-value 'reka-floating-frame-name buffer)))
                                   collect
                                   (with-current-buffer buffer
                                     (let ((geometry (or reka-floating-geometry
                                                         (list 0 0 (car reka-floating-default-size)
                                                               (cdr reka-floating-default-size)))))
                                       (apply #'reka-make-window-parameters
                                              reka-window frame-name geometry)))))))))

(defun reka--create-buffer (window &optional floating parent)
  (let ((buffer (get-buffer-create (make-temp-name "reka-window-"))))
    (with-current-buffer buffer
      (reka-mode)
      (setq-local reka-window window
                  reka-floating floating
                  reka-parent-window parent
                  reka-floating-frame-name (frame-parameter nil 'name)
                  reka--floating-tab (reka--current-tab-id))
      (reka--inherit-parent-location)
      (run-hooks 'reka-window-created-hook))
    (if floating
        (progn
          (setq reka--focused-floating-buffer buffer)
          (reka--remember-floating-selection))
      ;; A newly launched application must receive keyboard focus as well as
      ;; a tile.  `display-buffer' alone leaves the previous buffer selected.
      (when-let* ((tile (display-buffer buffer)))
        (select-window tile 'norecord)))
    (reka--refresh-floating-keymap)
    buffer))

(defun reka--update-focus-request (&rest _) ;; TODO: take & forward frame for active output logic
  "Send a focus request to the WM reflecting the current selected-window."
  (reka--refresh-floating-keymap)
  (when reka-handle
    (let* ((win (selected-window))
           (buf (or (reka--active-application-buffer) (window-buffer win)))
           ;; collection of conditions for "no interactive command in progress"
           ;; this is based on what which-key does + whacking moles as bugs show up
           (can-focus-window (and (reka--is-reka-buffer buf) ;; only focus wayland surfaces
                                  (not this-command) ;; not in an interactive command
                                  (= 0 (length unread-command-events)) ;; no pending key injections
                                  (= 0 (length (this-single-command-keys))) ;; no unfinished command sequence
                                  (= 0 (minibuffer-depth)) ;; no minibuffer editing active
                                  (= 0 (recursion-depth))))) ;; no recursive edit ongoing
      (unless (equal (list buf can-focus-window (selected-frame)) reka--last-focused)
        (setq reka--last-focused (list buf can-focus-window (selected-frame)))
        (reka-set-focus-request reka-handle
                                (when can-focus-window
                                  (buffer-local-value 'reka-window buf)))))))

(defun reka--selection-changed (&rest arguments)
  "Track an explicit Emacs selection separately from native floating focus."
  (unless (or reka--adjusting-floating-layout
              (and (buffer-live-p reka--focused-floating-buffer)
                   (or (equal reka--floating-selection-context
                              (reka--selected-context))
                       (and (> (minibuffer-depth) 0)
                            (window-minibuffer-p (selected-window))))))
    (setq reka--focused-floating-buffer nil
          reka--floating-keymap-active nil)
    (when (and (derived-mode-p 'reka-mode) reka-floating)
      (reka--detach-floating-buffer (current-buffer))))
  (apply #'reka--update-focus-request arguments))

(defconst reka--modifier-bits
  ;; TODO: can we handle this with XKB on the rust side, too?
  '((shift   . 1)
    (control . 4)
    (meta    . 8)
    (super   . 64)
    (hyper   . 128))
  "Modifier bits as per river_seat_v1.modifiers / XKB")

(defun reka--key-to-xkb (key-string)
  "Decomposes an Emacs key-string (e.g. `C-x') into (event key modifiers).
The Rust side resolves the keysyms using xkbcommon."
  (let* ((event (aref (kbd key-string) 0))
         (basic (event-basic-type event))
         (mods (seq-map (lambda (mod) (alist-get mod reka--modifier-bits))
                        (event-modifiers event)))
         (key (if (characterp basic) basic (symbol-name basic))))
    (list event key (apply #'logior mods))))

(defun reka-push-intercept-prefix (prefix &optional command)
  "Register PREFIX (in the format as expected by `kbd') as an intercept key
binding, meaning that it is always forwarded to Emacs. Use this in
combination with `global-set-key' to define global key bindings that are
always sent to Emacs.

COMMAND can be `toggle-fullscreen' or `toggle-floating' for native actions."
  (let ((data (reka--key-to-xkb prefix)))
    (reka-register-xkb-prefix reka-handle (if (integerp (car data))
                                              (car data)
                                            (symbol-name (car data)))
                              (cadr data) (caddr data) command)))

(defun reka-push-intercept-prefixes () ;; TODO: remove, leave only one way?
  "Update the intercept prefixes defined in `reka-intercept-prefixes'."
  (dolist (prefix reka-intercept-prefixes)
    (reka-push-intercept-prefix prefix)))

(defcustom reka-intercept-prefixes
  '("C-x" "C-u" "C-h" "M-x")
  "Prefix keys that should always go to Emacs."
  :type '(repeat key)
  :set (lambda (sym val)
         (set-default sym val)
         (when reka-handle (reka-push-intercept-prefixes))))

(defun reka--suppress-focus-event (_orig-fn _event)
  "No-op for suppressing certain focus events in advice."
  (interactive "e")
  ;; okay, *almost* no-op ...
  (setq reka--last-focused nil))

(defun reka--buffer-predicate (buffer)
  "Buffer predicate to avoid accidentally showing the same reka buffer twice."
  (or (not (with-current-buffer buffer (derived-mode-p 'reka-mode)))
      (and (not (buffer-local-value 'reka-floating buffer))
           (not (get-buffer-window buffer t)))))

(defun reka--split-window-advice (new-window)
  "Advice window splits to always display another buffer, if a reka buffer
was split."
  (with-selected-window new-window
    (with-current-buffer (window-buffer)
      (when (derived-mode-p 'reka-mode)
        (switch-to-buffer (other-buffer)))))
  new-window)

(defun reka--set-window-buffer-advice (orig win buf &rest r)
  "Avoid double-display of reka buffers, by stealing them if they are
visible elsewhere. Note that displaying the same buffer in two different
tabs, for example, is completely valid."
  (with-current-buffer buf
    (when (derived-mode-p 'reka-mode)
      (dolist (other (get-buffer-window-list buf nil 'visible))
        (unless (equal (or win (selected-window)) other)
          (with-selected-window other
            (switch-to-buffer (other-buffer)))))))

  (apply orig win buf r))

(defun reka-enable ()
  "Enable the reka window manager for river. Call this function once when
starting Emacs inside of river."
  (setq reka-ready nil)
  ;; TODO: this is a hack for lack of ability to figure out alignment ...
  (menu-bar-mode 0)
  (tool-bar-mode 0)

  ;; configure this and all future frames ..
  (let ((frame-params '((undecorated . t)
                        (buffer-predicate . reka--buffer-predicate))))
    (modify-all-frames-parameters frame-params))

  (advice-add 'split-window-below :filter-return #'reka--split-window-advice)
  (advice-add 'split-window-right :filter-return #'reka--split-window-advice)
  (advice-add 'set-window-buffer :around #'reka--set-window-buffer-advice)

  (message "Launching native module ...")
  (reka--ensure-frame-names)
  (add-to-list 'after-make-frame-functions #'reka--set-frame-name)
  (let ((pipe (make-pipe-process :name "reka-events"
                                 :buffer " *reka-events*"
                                 :filter #'reka--handle-event
                                 :noquery t)))
    (setq reka-handle (reka-start-wm pipe)))
  (reka-push-intercept-prefixes)
  (add-to-list 'emulation-mode-map-alists 'reka--floating-emulation-map-alist)
  (reka-configure-floating reka-handle
                           (car reka-floating-default-size)
                           (cdr reka-floating-default-size))
  (when reka-floating-toggle-key
    (reka-push-intercept-prefix reka-floating-toggle-key 'toggle-floating))

  ;; Layout signals
  (add-hook 'tab-bar-tab-post-open-functions #'reka--new-tab)
  (add-hook 'tab-bar-tab-post-select-functions #'reka--tab-selected)
  (add-hook 'window-configuration-change-hook #'reka--handle-event)

  ;; Focus signals
  (add-hook 'window-selection-change-functions #'reka--selection-changed)
  (add-hook 'window-buffer-change-functions    #'reka--selection-changed)
  (add-hook 'minibuffer-setup-hook             #'reka--update-focus-request)
  (add-hook 'minibuffer-exit-hook              #'reka--update-focus-request)
  (add-hook 'post-command-hook                 #'reka--update-focus-request)

  ;; Suppress pgtk focus feedback loop (as does EXWM)
  ;; TODO: figure out if/how this breaks multi-frame focus changes ...
  (advice-add 'handle-focus-in  :around #'reka--suppress-focus-event)
  (advice-add 'handle-focus-out :around #'reka--suppress-focus-event)

  (run-hooks 'reka-enable-hook))

(provide 'reka)
