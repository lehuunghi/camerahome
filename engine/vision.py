"""Require sustained temporal movement, not just a difference from the background."""
import cv2
import numpy as np
cv2.setNumThreads(1)


class MotionDetector:
    def __init__(self):
        self.background=None
        self.previous=None
        self.frames=0
        self.consecutive=0

    @staticmethod
    def area(difference,threshold,opening=False):
        # Remove a global exposure shift before looking for localized changes.
        difference=cv2.subtract(difference,np.full_like(difference,int(np.median(difference))))
        mask=cv2.threshold(difference,threshold,255,cv2.THRESH_BINARY)[1]
        kernel=np.ones((3,3),np.uint8)
        if opening:mask=cv2.morphologyEx(mask,cv2.MORPH_OPEN,kernel)
        else:mask=cv2.dilate(mask,kernel,iterations=1)
        contours,_=cv2.findContours(mask,cv2.RETR_EXTERNAL,cv2.CHAIN_APPROX_SIMPLE)
        return max((cv2.contourArea(c) for c in contours),default=0)

    def feed(self,frame):
        gray=cv2.cvtColor(frame,cv2.COLOR_BGR2GRAY) if frame.ndim==3 else frame
        blur=cv2.GaussianBlur(gray,(5,5),0)
        self.frames+=1
        if self.background is None:
            self.background=blur.astype(np.float32);self.previous=blur.copy()
            return False,0.
        foreground=self.area(cv2.absdiff(blur,cv2.convertScaleAbs(self.background)),22,True)
        changed=self.area(cv2.absdiff(blur,self.previous),16)
        moving=foreground>180 and changed>80 and self.frames>15
        self.consecutive=self.consecutive+1 if moving else 0
        triggered=self.consecutive>=3
        if triggered:self.consecutive=0
        cv2.accumulateWeighted(blur,self.background,.04)
        self.previous=blur.copy()
        return triggered,changed/gray.size
